//! Execution-trace composition (`ports/trace.rs`, store
//! `adapters/outbound/trace_store.rs`): which recording a process writes and
//! where readers find it.
//!
//! | Helper | Builds |
//! |---|---|
//! | [`open_sink`] | a new recording of the runner name (`--sandbox`, else `default`) under `<TENGU_HOME>/logs/trace/`, `config_hash` = `Config::source_sha256`, redacting with the process `SecretRegistry`; fail-soft: an unwritable dir = `NoopTrace` + a warn, the run goes on |
//! | [`run_ids`] | `runtime_id` + `run_id` for `AuditLog` (`decisions.jsonl`) |
//! | [`reader`] · [`run_path`] | a sandbox's `JsonlTraceReader` · one run's file |
//!
//! Who records (ST-11): `tengu run` (`RunKind::Run`, `runtime_id` = the lease
//! holder, `bootstrap/runtime.rs::start`), `tengu decide` (`RunKind::Decide`,
//! no `runtime_id`). `tengu webhooks` does not record yet.

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
use crate::domain::trace::{RunIds, RunKind};
use crate::ports::trace::TraceSink;

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

/// The ids every audit line of this recording carries.
pub(crate) fn run_ids(trace: &dyn TraceSink, runtime_id: Option<&str>) -> RunIds {
    RunIds {
        runtime_id: runtime_id.map(str::to_string),
        run_id: trace.run_id().map(str::to_string),
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
