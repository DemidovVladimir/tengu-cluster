//! Execution-trace plumbing the instrumentation sites share
//! (`TENGU_STUDIO_PLAN.md` ST-12; envelope `domain/trace.rs`, port
//! `ports/trace.rs`): which event caused what a task does now, and the
//! tool-call decorator.
//!
//! | Piece | Rule |
//! |---|---|
//! | [`Cause`] · [`caused_by`] · [`cause`] | the event that caused what this task does now (its `parent_event_id`) + the session / correlation it belongs to; set around a loop event (`LoopDispatch`: `loop.started`), a decide run (`trigger.*`), a loop's tool call (`action.selected`), a feed's calls and tick (`feed.fired`). Read where the next event is written: `LoopDispatch::enqueue`, `DecisionLoop::run_event`, [`TracedExecutor`]. A task it spawns does not inherit it |
//! | [`TracedExecutor`] | wraps one agent's executor (inside `egress::AttributedExecutor`, outside `SanitizedToolExecutor`: it sees redacted results): `tool.started` (`Running`, `{tool, args}`) → `tool.completed` (`Ok`) or `tool.failed` (`Failed`: the executor's error, or a typed result whose status is not usable) with `duration_ms`; `node_id` `tool:<agent>/<tool>`, `call_id` = `ToolCall.id`, parent / session / correlation = the [`cause`]; the result as a summary (`line1` of the text, a typed result's `key` / `status` / `headline`), never the whole text |
//!
//! The decorator reports what the executor returned; what the caller makes
//! of it — a loop's `ok`, a feed's error class and backoff — is the caller's
//! own event (`action.completed`, `feed.failed` / `feed.retrying`), so no
//! rule is computed twice.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::domain::message::{Message, ToolCall};
use crate::domain::trace::{Component, EventDraft, Status};
use crate::domain::workflow::node_id;
use crate::ports::engine::ToolExecutor;
use crate::ports::tool::ToolOutput;
use crate::ports::trace::TraceSink;

/// What caused the work a task does now (module table).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Cause {
    /// `event_id` of the causing event.
    pub parent: Option<String>,
    pub session: Option<String>,
    /// `None` = the session's (`RunContext::stamp`).
    pub correlation: Option<String>,
}

impl Cause {
    pub(crate) fn new(parent: Option<String>, session: impl Into<String>) -> Self {
        Self {
            parent,
            session: Some(session.into()),
            correlation: None,
        }
    }

    pub(crate) fn correlation(mut self, id: impl Into<String>) -> Self {
        self.correlation = Some(id.into());
        self
    }

    /// `d` with this cause's parent / session / correlation where `d` has none.
    pub(crate) fn fill(&self, mut d: EventDraft) -> EventDraft {
        if d.parent_event_id.is_none() {
            d.parent_event_id = self.parent.clone();
        }
        if d.session_id.is_none() {
            d.session_id = self.session.clone();
        }
        if d.correlation_id.is_none() {
            d.correlation_id = self.correlation.clone();
        }
        d
    }
}

tokio::task_local! {
    static CAUSE: Cause;
}

/// Run `fut` with `cause` as what caused it (on this task).
pub(crate) async fn caused_by<F: Future>(cause: Cause, fut: F) -> F::Output {
    CAUSE.scope(cause, fut).await
}

/// Run `f` with `cause` as what caused it (synchronous callers).
pub(crate) fn caused_by_sync<R>(cause: Cause, f: impl FnOnce() -> R) -> R {
    CAUSE.sync_scope(cause, f)
}

/// The cause of the work running on this task, if any.
pub(crate) fn cause() -> Option<Cause> {
    CAUSE.try_with(Clone::clone).ok()
}

/// A tool executor whose every call writes `tool.*` events (module table).
pub(crate) struct TracedExecutor {
    inner: Arc<dyn ToolExecutor>,
    sink: Arc<dyn TraceSink>,
    agent: String,
    /// Where each call's full record lives (a loop: its `decisions.jsonl`,
    /// key = the call id); `None` = nowhere but the tool's own store.
    artifact: Option<String>,
}

impl TracedExecutor {
    pub(crate) fn new(
        inner: Arc<dyn ToolExecutor>,
        sink: Arc<dyn TraceSink>,
        agent: &str,
        artifact: Option<String>,
    ) -> Self {
        Self {
            inner,
            sink,
            agent: agent.to_string(),
            artifact,
        }
    }

    fn draft(&self, call: &ToolCall, kind: &str, status: Status) -> EventDraft {
        let mut d = EventDraft::new(Component::Tool, kind, status)
            .call(call.id.clone())
            .node(node_id::tool(&self.agent, &call.name));
        if let Some(file) = &self.artifact {
            d = d.artifact(file.clone(), Some(call.id.clone()));
        }
        match cause() {
            Some(c) => c.fill(d),
            None => d,
        }
    }

    fn started(&self, call: &ToolCall) -> Option<String> {
        let payload = json!({"tool": call.name, "args": call.arguments});
        self.sink.emit(
            self.draft(call, "tool.started", Status::Running)
                .payload(payload),
        )
    }

    fn finished(
        &self,
        call: &ToolCall,
        started: Option<String>,
        result: &anyhow::Result<ToolOutput>,
        ms: u64,
    ) {
        let (kind, status, payload) = match result {
            Err(e) => (
                "tool.failed",
                Status::Failed,
                json!({"tool": call.name, "error": format!("{e:#}")}),
            ),
            Ok(out) => {
                let line1 = out.text.lines().next().unwrap_or_default();
                let mut p = json!({
                    "tool": call.name,
                    "text_bytes": out.text.len(),
                    "line1": line1,
                });
                let mut usable = true;
                if let (Some(o), Value::Object(m)) = (&out.observation, &mut p) {
                    usable = o.status.usable();
                    m.insert(
                        "observation".into(),
                        json!({
                            "key": o.key,
                            "status": o.status.as_str(),
                            "headline": o.headline,
                            "errors": o.errors,
                        }),
                    );
                }
                if usable {
                    ("tool.completed", Status::Ok, p)
                } else {
                    ("tool.failed", Status::Failed, p)
                }
            }
        };
        let mut d = self.draft(call, kind, status).duration(ms).payload(payload);
        if let Some(id) = started {
            d = d.parent(id);
        }
        self.sink.emit(d);
    }
}

#[async_trait]
impl ToolExecutor for TracedExecutor {
    async fn execute(&self, call: &ToolCall, messages: &[Message]) -> anyhow::Result<String> {
        let started = self.started(call);
        let t0 = Instant::now();
        let result = self
            .inner
            .execute(call, messages)
            .await
            .map(ToolOutput::from);
        self.finished(call, started, &result, t0.elapsed().as_millis() as u64);
        result.map(|o| o.text)
    }

    async fn execute_typed(
        &self,
        call: &ToolCall,
        messages: &[Message],
    ) -> anyhow::Result<ToolOutput> {
        let started = self.started(call);
        let t0 = Instant::now();
        let result = self.inner.execute_typed(call, messages).await;
        self.finished(call, started, &result, t0.elapsed().as_millis() as u64);
        result
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::observation::{ObsSource, ObsStatus, Observation};
    use std::sync::Mutex;

    /// Keeps every draft it is handed; `event_id` = `t:<n>`.
    #[derive(Default)]
    pub(crate) struct MemTrace {
        pub drafts: Mutex<Vec<EventDraft>>,
    }

    impl MemTrace {
        pub(crate) fn kinds(&self) -> Vec<String> {
            self.drafts
                .lock()
                .unwrap()
                .iter()
                .map(|d| d.kind.clone())
                .collect()
        }

        pub(crate) fn all(&self) -> Vec<EventDraft> {
            self.drafts.lock().unwrap().clone()
        }

        /// The event id `emit` returned for draft `i` (0-based).
        pub(crate) fn id(i: usize) -> String {
            format!("t:{}", i + 1)
        }
    }

    impl TraceSink for MemTrace {
        fn emit(&self, draft: EventDraft) -> Option<String> {
            let mut d = self.drafts.lock().unwrap();
            d.push(draft);
            Some(format!("t:{}", d.len()))
        }

        fn run_id(&self) -> Option<&str> {
            Some("mem")
        }
    }

    struct Fixed(Result<ToolOutput, String>);

    #[async_trait]
    impl ToolExecutor for Fixed {
        async fn execute(&self, c: &ToolCall, m: &[Message]) -> anyhow::Result<String> {
            self.execute_typed(c, m).await.map(|o| o.text)
        }
        async fn execute_typed(&self, _c: &ToolCall, _m: &[Message]) -> anyhow::Result<ToolOutput> {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            self.0.clone().map_err(|e| anyhow::anyhow!(e))
        }
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({"path": "out/marker.txt"}),
        }
    }

    /// Every call: `tool.started` then `tool.completed` / `tool.failed`, the
    /// full call id, the tool node, the cause's parent + session, a duration
    /// on the finish, the finish's parent = the start.
    #[tokio::test]
    async fn tool_events_carry_call_id_and_duration() {
        let sink = Arc::new(MemTrace::default());
        let ok = TracedExecutor::new(
            Arc::new(Fixed(Ok(ToolOutput::from(
                "wrote 15 bytes\nmore".to_string(),
            )))),
            sink.clone(),
            "lab",
            Some("/home/x/logs/decisions.jsonl".into()),
        );
        let id = "demo:decide-demo-2b1f0c4e-6a55-4f1e-9a3c-7d2e8f9b0a1c:1";
        let cause = Cause::new(
            Some("run:7".into()),
            "decide-demo-2b1f0c4e-6a55-4f1e-9a3c-7d2e8f9b0a1c",
        );
        caused_by(cause, ok.execute_typed(&call(id, "write_file"), &[]))
            .await
            .unwrap();
        let failing = TracedExecutor::new(
            Arc::new(Fixed(Err("read in/absent.txt: No such file".into()))),
            sink.clone(),
            "lab",
            None,
        );
        assert!(failing
            .execute_typed(&call("feed:probe:1759912350000:0", "read_file"), &[])
            .await
            .is_err());
        let d = sink.all();
        assert_eq!(
            sink.kinds(),
            [
                "tool.started",
                "tool.completed",
                "tool.started",
                "tool.failed"
            ]
        );
        assert_eq!(d[0].call_id.as_deref(), Some(id));
        assert_eq!(d[0].node_id.as_deref(), Some("tool:lab/write_file"));
        assert_eq!(d[0].parent_event_id.as_deref(), Some("run:7"));
        assert_eq!(
            d[0].session_id.as_deref(),
            Some("decide-demo-2b1f0c4e-6a55-4f1e-9a3c-7d2e8f9b0a1c")
        );
        assert_eq!(d[0].payload["args"]["path"], json!("out/marker.txt"));
        assert_eq!(d[1].parent_event_id, Some(MemTrace::id(0)));
        assert_eq!(d[1].status, Status::Ok);
        assert!(d[1].duration_ms.unwrap() >= 5, "{:?}", d[1].duration_ms);
        assert_eq!(d[1].payload["line1"], json!("wrote 15 bytes"));
        assert_eq!(d[1].artifact.as_ref().unwrap().key.as_deref(), Some(id));
        assert_eq!(d[0].duration_ms, None, "a start has no duration");
        // No cause: no session / correlation; the failure carries the error.
        assert_eq!(d[2].session_id, None);
        assert_eq!(d[3].status, Status::Failed);
        assert_eq!(d[3].call_id.as_deref(), Some("feed:probe:1759912350000:0"));
        assert_eq!(d[3].node_id.as_deref(), Some("tool:lab/read_file"));
        assert_eq!(
            d[3].payload["error"],
            json!("read in/absent.txt: No such file")
        );
        assert!(d[3].duration_ms.is_some());
        assert!(d[3].artifact.is_none());
    }

    /// A typed result whose status is not usable is `tool.failed` with its
    /// key and errors; a usable one is `tool.completed`.
    #[tokio::test]
    async fn typed_error_rows_are_failures() {
        let obs = |status| Observation {
            key: "price_oracle/1:So11111111111111111111111111111111111111112".into(),
            schema: "price_oracle/1".into(),
            tool: "sol_price".into(),
            observed_at_ms: 0,
            slot: None,
            ttl_ms: 0,
            source: ObsSource::Live,
            status,
            errors: vec![],
            headline: "sol_price".into(),
            features: Default::default(),
            data: Value::Null,
        };
        let sink = Arc::new(MemTrace::default());
        for status in [ObsStatus::Error, ObsStatus::Partial] {
            let ex = TracedExecutor::new(
                Arc::new(Fixed(Ok(ToolOutput::observed(obs(status), 0)))),
                sink.clone(),
                "lab",
                None,
            );
            ex.execute_typed(&call("c", "sol_price"), &[])
                .await
                .unwrap();
        }
        let d = sink.all();
        assert_eq!(d[1].kind, "tool.failed");
        assert_eq!(d[1].payload["observation"]["status"], json!("error"));
        assert_eq!(
            d[1].payload["observation"]["key"],
            json!("price_oracle/1:So11111111111111111111111111111111111111112")
        );
        assert_eq!(d[3].kind, "tool.completed");
    }
}
