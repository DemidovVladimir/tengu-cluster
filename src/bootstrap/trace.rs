//! Execution-trace composition (`ports/trace.rs`, store
//! `adapters/outbound/trace_store.rs`): which recording a process writes and
//! where readers find it.
//!
//! | Helper | Builds |
//! |---|---|
//! | [`open_sink`] | a new recording of the runner name (`--sandbox`, else `default`) under `<TENGU_HOME>/logs/trace/`, `config_hash` = `Config::source_sha256`, redacting with the process `SecretRegistry`; fail-soft: an unwritable dir = `NoopTrace` + a warn, the run goes on |
//! | [`open_orchestration`] · [`close`] | a chat / telegram / eval recording, opened only with `[orchestrator]` (their plans are all they record) · its last event, `run.closed` |
//! | [`run_ids`] · [`Recording`] | `runtime_id` + `run_id` for `AuditLog` (`decisions.jsonl`) · the sink + ids handed to each decision loop (`bootstrap::decision`) |
//! | [`reader`] · [`run_path`] | a sandbox's `JsonlTraceReader` · one run's file |
//!
//! Who records: `tengu run` (`RunKind::Run`, `runtime_id` = the lease holder,
//! `bootstrap/runtime.rs::start`: `runtime.*`, `feed.*`, `loop.*`, every
//! loop's step / tool events, each webhook request's `trigger.webhook` and
//! its work, escalation turns), `tengu decide` (`RunKind::Decide`, no
//! `runtime_id`: `trigger.*` + the loop's step / tool events), `tengu
//! webhooks` (`RunKind::Webhooks`, `runtime_id` = the lease holder: the same
//! per-request events, `run.closed`), `tengu chat` / `tengu telegram` (one
//! per process) and `tengu eval` (one per orchestrated row) with
//! `[orchestrator]` (`plan.*`, `step.*`, `metrics.recorded`, each step's
//! `run-agent` `tool.*`; `application/orchestrator/trace.rs`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tracing::{info, warn};

use crate::adapters::outbound::noop::NoopTrace;
use crate::adapters::outbound::trace_store::{
    run_file, trace_root, JsonlTraceReader, JsonlTraceSink,
};
use crate::config::Config;
use crate::domain::secrets::SecretRegistry;
use crate::domain::trace::{Component, EventDraft, RunIds, RunKind, Status, RUN_CLOSED};
use crate::ports::trace::TraceSink;
use serde_json::json;

fn root() -> PathBuf {
    trace_root(&crate::config::paths::resolve_tengu_home())
}

/// Start a recording for this process (module table).
pub(crate) fn open_sink(
    config: &Config,
    runtime_id: Option<&str>,
    kind: RunKind,
    secrets: &Arc<SecretRegistry>,
) -> Arc<dyn TraceSink> {
    let sandbox = crate::bootstrap::runtime::runner_name(config);
    match JsonlTraceSink::open(
        &root(),
        &sandbox,
        config.source_sha256.as_deref(),
        runtime_id,
        kind,
        Arc::clone(secrets),
    ) {
        Ok(sink) => {
            info!(
                run_id = sink.run_id().unwrap_or_default(),
                path = %sink.path().display(),
                "trace recording"
            );
            Arc::new(sink)
        }
        Err(e) => {
            let error = format!("{e:#}");
            warn!(%sandbox, %error, "trace unavailable; this run records no trace");
            Arc::new(NoopTrace)
        }
    }
}

/// A surface's recording of its orchestration (module table); `None`
/// without `[orchestrator]`.
pub(crate) fn open_orchestration(
    config: &Config,
    kind: RunKind,
    secrets: &Arc<SecretRegistry>,
) -> Option<Arc<dyn TraceSink>> {
    config.orchestrator.as_ref()?;
    Some(open_sink(config, None, kind, secrets))
}

/// The last event of a chat / telegram / eval / webhooks recording.
pub(crate) fn close(sink: &dyn TraceSink, reason: &str, failed: bool) {
    let status = if failed { Status::Failed } else { Status::Ok };
    sink.emit(
        EventDraft::new(Component::Runtime, RUN_CLOSED, status)
            .payload(json!({"reason": reason, "failed": failed})),
    );
}

/// The ids every audit line of this recording carries.
pub(crate) fn run_ids(trace: &dyn TraceSink, runtime_id: Option<&str>) -> RunIds {
    RunIds {
        runtime_id: runtime_id.map(str::to_string),
        run_id: trace.run_id().map(str::to_string),
    }
}

/// One process's recording as the composition hands it to what it builds
/// (decision loops, feeds): the sink their events go to + the ids their
/// audit lines carry. [`Default`] = records nothing (`NoopTrace`, no ids):
/// tests.
#[derive(Clone)]
pub(crate) struct Recording {
    pub sink: Arc<dyn TraceSink>,
    pub ids: RunIds,
}

impl Recording {
    pub(crate) fn of(sink: Arc<dyn TraceSink>, runtime_id: Option<&str>) -> Self {
        let ids = run_ids(&*sink, runtime_id);
        Self { sink, ids }
    }
}

impl Default for Recording {
    fn default() -> Self {
        Self {
            sink: Arc::new(NoopTrace),
            ids: RunIds::default(),
        }
    }
}

/// The recordings of `sandbox`.
pub(crate) fn reader(sandbox: &str) -> Result<JsonlTraceReader> {
    JsonlTraceReader::new(&root(), sandbox)
}

/// `<TENGU_HOME>/logs/trace/<sandbox>/<run_id>.jsonl`.
pub(crate) fn run_path(sandbox: &str, run_id: &str) -> PathBuf {
    run_file(&root(), sandbox, run_id)
}
