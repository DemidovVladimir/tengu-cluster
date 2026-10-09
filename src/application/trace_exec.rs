//! Execution-trace plumbing the instrumentation sites share
//! (`TENGU_STUDIO_PLAN.md` ST-12; envelope `domain/trace.rs`, port
//! `ports/trace.rs`): which event caused what a task does now, and the
//! tool-call decorator.
//!
//! | Piece | Rule |
//! |---|---|
//! | [`Cause`] · [`caused_by`] · [`cause`] | the event that caused what this task does now (its `parent_event_id`) + the session / correlation it belongs to; set around a loop event (`LoopDispatch`: `loop.started`), a decide run (`trigger.*`), a loop's tool call (`action.selected`), a feed's calls and tick (`feed.fired`). Read where the next event is written: `LoopDispatch::enqueue`, `DecisionLoop::run_event`, [`TracedExecutor`]. A task it spawns does not inherit it |
//! | [`TracedExecutor`] | wraps one agent's executor (inside `egress::AttributedExecutor`, outside `SanitizedToolExecutor`: it sees redacted results): `tool.started` (`Running`, `{tool, args}`) → `tool.completed` (`Ok`) or `tool.failed` (`Failed`: the executor's error, a typed result whose status is not usable, or an `http_request` text whose status line is not 2xx — `reduce::http_ok`, the rule a loop's `ok` uses) with `duration_ms`; `node_id` `tool:<agent>/<tool>`, `call_id` = `ToolCall.id`, parent / session / correlation = the [`cause`]; the result as a summary (`line1` of the text, a typed result's `key` / `status` / `headline`), never the whole text |
//! | [`DraftBuffer`] · [`replay_child`] | a `run-agent` child's sink (`AgentIpcInput.trace` set): keeps its `tool.*` drafts (child clock, parent = the step's `step.started` for a root, ids `ipc:<n>`, redacted + bounded, ≤ [`MAX_CHILD_EVENTS`]) for `AgentIpcOutput.trace` · the parent writes them into its own recording in order, each `ipc:<n>` parent mapped to the id its sink gave, the session = the step's, anything but `tool.*` dropped — one writer per run file |
//!
//! The decorator reports what the executor returned, judged by the shared
//! pass / fail predicates (`ObsStatus::usable`, `reduce::http_ok`); what the
//! caller makes of it — a loop's history entry, a feed's error class and
//! backoff — is the caller's own event (`action.completed`, `feed.failed` /
//! `feed.retrying`), so no rule is computed twice.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::application::decision_loop::reduce::http_ok;
use crate::domain::message::{Message, ToolCall};
use crate::domain::observation::now_ms;
use crate::domain::secrets::SecretRegistry;
use crate::domain::trace::{
    bound_payload, scrub_value, Component, EventDraft, Status, MAX_PAYLOAD_BYTES,
};
use crate::domain::workflow::node_id;
use crate::ports::engine::ToolExecutor;
use crate::ports::tool::ToolOutput;
use crate::ports::trace::TraceSink;

/// Drafts a `run-agent` child hands back at most ([`DraftBuffer`]).
pub(crate) const MAX_CHILD_EVENTS: usize = 512;
/// Prefix of a child's own event ids ([`DraftBuffer`]); never a real one
/// (`<run_id>:<seq>`, a UUID).
const CHILD_ID: &str = "ipc:";

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
                // Text: a non-2xx `http_request` status line fails the call,
                // as it does a loop's `ok` and a feed's outcome.
                let mut usable = http_ok(line1).unwrap_or(true);
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

/// A `run-agent` child's trace sink (module table): the step's tool events,
/// kept for the parent.
pub(crate) struct DraftBuffer {
    /// The step's `step.started` in the parent's recording.
    parent: String,
    session: String,
    secrets: Arc<SecretRegistry>,
    drafts: Mutex<Vec<EventDraft>>,
    dropped: AtomicU64,
}

impl DraftBuffer {
    pub(crate) fn new(parent: String, session: String, secrets: Arc<SecretRegistry>) -> Self {
        Self {
            parent,
            session,
            secrets,
            drafts: Mutex::new(Vec::new()),
            dropped: AtomicU64::new(0),
        }
    }

    /// Every draft kept, in emit order (`AgentIpcOutput.trace`); a warn
    /// names how many did not fit.
    pub(crate) fn take(&self) -> Vec<EventDraft> {
        let dropped = self.dropped.load(Ordering::Relaxed);
        if dropped > 0 {
            tracing::warn!(
                dropped,
                kept = MAX_CHILD_EVENTS,
                "step trace: events past the cap not handed back"
            );
        }
        std::mem::take(&mut *self.drafts.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

impl TraceSink for DraftBuffer {
    fn emit(&self, mut d: EventDraft) -> Option<String> {
        if d.parent_event_id.is_none() {
            d.parent_event_id = Some(self.parent.clone());
        }
        if d.session_id.is_none() {
            d.session_id = Some(self.session.clone());
        }
        if d.ts_ms.is_none() {
            d = d.at(now_ms());
        }
        // The parent's sink redacts and bounds again; here it keeps the IPC
        // payload small and the child's own secrets out of it.
        scrub_value(&mut d.payload, &self.secrets);
        d.payload = bound_payload(std::mem::take(&mut d.payload), MAX_PAYLOAD_BYTES, d.keep).0;
        let mut v = self.drafts.lock().unwrap_or_else(|p| p.into_inner());
        if v.len() >= MAX_CHILD_EVENTS {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        v.push(d);
        Some(format!("{CHILD_ID}{}", v.len()))
    }

    /// The child writes into the parent's run, not one of its own.
    fn run_id(&self) -> Option<&str> {
        None
    }
}

/// Write a child's drafts into `sink` (module table); how many were written.
pub(crate) fn replay_child(
    sink: &dyn TraceSink,
    drafts: Vec<EventDraft>,
    step_event: &str,
    session: &str,
) -> usize {
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut written = 0;
    for (i, mut d) in drafts.into_iter().enumerate() {
        let own = format!("{CHILD_ID}{}", i + 1);
        if !d.kind.starts_with("tool.") {
            continue;
        }
        d.parent_event_id = Some(
            d.parent_event_id
                .as_ref()
                .and_then(|p| ids.get(p))
                .cloned()
                .unwrap_or_else(|| step_event.to_string()),
        );
        d.session_id = Some(session.to_string());
        d.correlation_id = None;
        d.component = Component::Tool;
        if let Some(id) = sink.emit(d) {
            ids.insert(own, id);
            written += 1;
        }
    }
    written
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::observation::{ObsSource, ObsStatus, Observation};

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

    /// A child's tool events cross the IPC and land in the parent's run:
    /// the start under the step, the finish under the start's real id, the
    /// step's session, the child's clock; a secret never leaves the child;
    /// anything but `tool.*` is dropped; past the cap nothing is kept.
    #[tokio::test]
    async fn child_tool_events_replay_under_the_step() {
        let mut secrets = SecretRegistry::new();
        secrets.register("sk-or-v1-0123456789abcdef".into());
        let buf = Arc::new(DraftBuffer::new(
            "run:7".into(),
            "chat-session".into(),
            Arc::new(secrets),
        ));
        let ex = TracedExecutor::new(
            Arc::new(Fixed(Ok(ToolOutput::from("4 pools".to_string())))),
            buf.clone(),
            "crypto_researcher",
            None,
        );
        let mut c = call("call_1", "dlmm_pools");
        c.arguments = json!({"key": "sk-or-v1-0123456789abcdef"});
        ex.execute_typed(&c, &[]).await.unwrap();
        buf.emit(EventDraft::new(
            Component::Loop,
            "loop.started",
            Status::Running,
        ));
        let drafts = buf.take();
        assert_eq!(drafts.len(), 3);
        assert_eq!(drafts[1].parent_event_id.as_deref(), Some("ipc:1"));
        assert!(drafts[0].ts_ms.is_some(), "the child's clock");
        let wire = serde_json::to_string(&drafts).unwrap();
        assert!(!wire.contains("sk-or-v1-0123456789abcdef"), "{wire}");
        let back: Vec<EventDraft> = serde_json::from_str(&wire).unwrap();

        let parent = MemTrace::default();
        assert_eq!(replay_child(&parent, back, "run:7", "chat-session"), 2);
        let d = parent.all();
        assert_eq!(parent.kinds(), ["tool.started", "tool.completed"]);
        assert_eq!(d[0].parent_event_id.as_deref(), Some("run:7"));
        assert_eq!(d[1].parent_event_id, Some(MemTrace::id(0)));
        assert_eq!(
            d[0].node_id.as_deref(),
            Some("tool:crypto_researcher/dlmm_pools")
        );
        assert!(d
            .iter()
            .all(|e| e.session_id.as_deref() == Some("chat-session")));
        assert_eq!(d[1].payload["line1"], json!("4 pools"));

        let full = DraftBuffer::new("p".into(), "s".into(), Arc::new(SecretRegistry::new()));
        for _ in 0..MAX_CHILD_EVENTS + 3 {
            full.emit(EventDraft::new(
                Component::Tool,
                "tool.started",
                Status::Running,
            ));
        }
        assert_eq!(full.take().len(), MAX_CHILD_EVENTS);
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

    /// An `http_request` text whose status line is not 2xx is `tool.failed`
    /// — the verdict a loop's `ok` (`parse_tool_output`) gives it; a 2xx
    /// status and plain text are `tool.completed`.
    #[tokio::test]
    async fn http_status_line_decides_text_results() {
        let sink = Arc::new(MemTrace::default());
        for text in [
            "HTTP 503 https://api.example/x\n{\"error\":\"down\"}",
            "HTTP 200 https://api.example/x\n{}",
            "File 'out/marker.txt' written (12 bytes)",
        ] {
            let ex = TracedExecutor::new(
                Arc::new(Fixed(Ok(ToolOutput::from(text.to_string())))),
                sink.clone(),
                "lab",
                None,
            );
            let out = ex.execute_typed(&call("c", "http_request"), &[]).await;
            let (ok, _) =
                crate::application::decision_loop::reduce::parse_tool_output(&out.unwrap().text);
            let finish = sink.all().pop().unwrap();
            let want = if ok { "tool.completed" } else { "tool.failed" };
            assert_eq!(finish.kind, want, "{text}");
        }
        let kinds = sink.kinds();
        assert_eq!(
            [&kinds[1], &kinds[3], &kinds[5]],
            ["tool.failed", "tool.completed", "tool.completed"]
        );
        assert_eq!(
            sink.all()[1].payload["line1"],
            json!("HTTP 503 https://api.example/x")
        );
    }
}
