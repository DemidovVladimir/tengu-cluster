//! Execution-trace ports (`domain/trace.rs`, `TENGU_STUDIO_PLAN.md` § 5).
//! Impl: `adapters::outbound::trace_store` (JSONL per run under
//! `<TENGU_HOME>/logs/trace/<sandbox>/`), `adapters::outbound::noop::NoopTrace`.
//!
//! | Port | Contract |
//! |---|---|
//! | [`TraceSink`] | one recording: stamps `seq` / ids / time, redacts and bounds the payload, writes each event once, in `seq` order; fail-soft (a trace error never stops a run) |
//! | [`TraceReader`] | one sandbox's recordings: list runs, page events after a `seq`, follow a run live (bounded channel: a slow reader waits, loses nothing, resumes by `seq`) |

use anyhow::Result;
use tokio::sync::mpsc::Receiver;

use crate::domain::trace::{EventDraft, ExecutionEvent, RunSummary};

pub(crate) trait TraceSink: Send + Sync {
    /// Write one event; its `event_id`, or `None` when nothing was written
    /// (a no-op sink, an IO error — logged, never raised). Called by the
    /// instrumentation sites (ST-12); `run.opened` is written by the store.
    #[cfg_attr(not(test), allow(dead_code))]
    fn emit(&self, draft: EventDraft) -> Option<String>;
    /// This recording's `run_id`; `None` for a sink that records nothing.
    fn run_id(&self) -> Option<&str>;
}

pub(crate) trait TraceReader: Send + Sync {
    /// Every run of the sandbox, oldest first.
    fn runs(&self) -> Result<Vec<RunSummary>>;
    /// Up to `limit` events of `run_id` with `seq > after_seq`, in `seq`
    /// order. A partial last line (a write in progress) is not returned.
    fn events(&self, run_id: &str, after_seq: u64, limit: usize) -> Result<Vec<ExecutionEvent>>;
    /// Every event of `run_id` with `seq > after_seq`, then each new one as
    /// it is written, until the receiver is dropped. Needs a tokio runtime.
    fn follow(&self, run_id: &str, after_seq: u64) -> Result<Receiver<ExecutionEvent>>;
}
