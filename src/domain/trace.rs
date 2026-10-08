//! Execution trace — the versioned, correlated event envelope every runtime
//! component writes once (`TENGU_STUDIO_PLAN.md` § 5) and Studio / `tengu
//! trace` read back. Pure data + ordering / bounding rules; the JSONL store
//! is `adapters/outbound/trace_store.rs`, the port `ports/trace.rs`.
//!
//! | Field | Value |
//! |---|---|
//! | `schema_version` | [`TRACE_SCHEMA_VERSION`] |
//! | `event_id` | `<run_id>:<seq>` ([`event_id`]) — the same live and in replay |
//! | `seq` | 1, 2, … per run, in file order (the sink stamps it under one lock) |
//! | `ts_ms` | the sink's wall clock, or the draft's own (a replay's clock) |
//! | `sandbox` · `config_hash` | runner name · `Config::source_sha256` |
//! | `runtime_id` | the `tengu run` lease holder `<host>:<pid>:<uuid>`; `None` for `tengu decide` |
//! | `run_id` | a fresh UUID v4 per recording (one `tengu run` process, one `tengu decide`) — names the file |
//! | `session_id` | the event's session: `<feed>:<slot ms>` · `decide-<loop>-<uuid>` · `webhook-<endpoint>-<uuid>` |
//! | `correlation_id` | the draft's, else `session_id`, else `runtime_id`, else `run_id` ([`RunContext::stamp`]) |
//! | `parent_event_id` · `call_id` | the causing event · the tool call id (`{loop}:{session}:{t}`, `feed:<n>:<slot>:<i>`) |
//! | `component` · `kind` · `node_id` · `status` | [`Component`] · dotted `<family>.<what>` (`run.opened`, `jev.completed`, …) · a `domain::workflow` node id · [`Status`] |
//! | `duration_ms` · `payload` · `artifact` | wall time of the thing reported · a redacted object ≤ [`MAX_PAYLOAD_BYTES`] ([`bound_payload`]) · where the full record lives ([`ArtifactRef`]) |
//!
//! Every field is always present (`null` when unknown), so a reader sees one
//! shape; unknown fields are ignored, so an older reader takes newer lines.
//! The first event of every run is [`RUN_OPENED`] (payload `{kind, pid}`).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Version of the [`ExecutionEvent`] JSON shape.
pub(crate) const TRACE_SCHEMA_VERSION: u32 = 1;
/// Largest payload a sink writes (serialized bytes); bigger ones lose whole
/// fields ([`bound_payload`]).
pub(crate) const MAX_PAYLOAD_BYTES: usize = 4096;
/// `kind` of the first event of every run.
pub(crate) const RUN_OPENED: &str = "run.opened";

/// One event (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ExecutionEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub seq: u64,
    pub ts_ms: i64,
    pub sandbox: String,
    #[serde(default)]
    pub config_hash: Option<String>,
    #[serde(default)]
    pub runtime_id: Option<String>,
    pub run_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    pub correlation_id: String,
    #[serde(default)]
    pub parent_event_id: Option<String>,
    #[serde(default)]
    pub call_id: Option<String>,
    pub component: Component,
    pub kind: String,
    #[serde(default)]
    pub node_id: Option<String>,
    pub status: Status,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub artifact: Option<ArtifactRef>,
}

/// Where the full record of an event lives (a large or raw result stays
/// there; the event carries a bounded summary).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ArtifactRef {
    /// A path under `<TENGU_HOME>` (`logs/decisions.jsonl`) or an absolute one.
    pub file: String,
    /// The record's key in it (a `call_id`, an observation key, a line's
    /// `decision_id`).
    #[serde(default)]
    pub key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Component {
    Runtime,
    Feed,
    Loop,
    Observation,
    Jev,
    Action,
    Tool,
    Gate,
    Orchestrator,
    Studio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Pending,
    Running,
    Ok,
    Failed,
    Refused,
    Escalated,
    Dropped,
    Stale,
    Missing,
    Skipped,
}

/// What a recording is (`run.opened` payload `kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunKind {
    /// One `tengu run` process.
    Run,
    /// One `tengu decide` (its own loop, outside the runtime lease).
    Decide,
    /// The Studio server's own events.
    Studio,
}

impl RunKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RunKind::Run => "run",
            RunKind::Decide => "decide",
            RunKind::Studio => "studio",
        }
    }
}

/// `<run_id>:<seq>`.
pub(crate) fn event_id(run_id: &str, seq: u64) -> String {
    format!("{run_id}:{seq}")
}

/// A recording's identity, as audit lines carry it (`decisions.jsonl`
/// `runtime_id` / `run_id`, both additive).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RunIds {
    pub runtime_id: Option<String>,
    pub run_id: Option<String>,
}

/// What the caller knows of an event; the sink stamps the rest
/// ([`RunContext::stamp`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EventDraft {
    pub component: Component,
    pub kind: String,
    pub status: Status,
    pub session_id: Option<String>,
    pub correlation_id: Option<String>,
    pub parent_event_id: Option<String>,
    pub call_id: Option<String>,
    pub node_id: Option<String>,
    pub duration_ms: Option<u64>,
    pub payload: Value,
    pub artifact: Option<ArtifactRef>,
    /// The caller's clock (replay); `None` = the sink's wall clock.
    pub ts_ms: Option<i64>,
}

// The builders beyond `new` / `payload` are for the instrumentation sites
// (TENGU_STUDIO_PLAN.md ST-12).
#[cfg_attr(not(test), allow(dead_code))]
impl EventDraft {
    pub(crate) fn new(component: Component, kind: impl Into<String>, status: Status) -> Self {
        Self {
            component,
            kind: kind.into(),
            status,
            session_id: None,
            correlation_id: None,
            parent_event_id: None,
            call_id: None,
            node_id: None,
            duration_ms: None,
            payload: Value::Object(Default::default()),
            artifact: None,
            ts_ms: None,
        }
    }
    pub(crate) fn session(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }
    pub(crate) fn correlation(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }
    pub(crate) fn parent(mut self, event_id: impl Into<String>) -> Self {
        self.parent_event_id = Some(event_id.into());
        self
    }
    pub(crate) fn call(mut self, id: impl Into<String>) -> Self {
        self.call_id = Some(id.into());
        self
    }
    pub(crate) fn node(mut self, id: impl Into<String>) -> Self {
        self.node_id = Some(id.into());
        self
    }
    pub(crate) fn duration(mut self, ms: u64) -> Self {
        self.duration_ms = Some(ms);
        self
    }
    pub(crate) fn payload(mut self, v: Value) -> Self {
        self.payload = v;
        self
    }
    pub(crate) fn artifact(mut self, file: impl Into<String>, key: Option<String>) -> Self {
        self.artifact = Some(ArtifactRef {
            file: file.into(),
            key,
        });
        self
    }
    pub(crate) fn at(mut self, ts_ms: i64) -> Self {
        self.ts_ms = Some(ts_ms);
        self
    }
}

/// The fields every event of one run shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunContext {
    pub sandbox: String,
    pub config_hash: Option<String>,
    pub runtime_id: Option<String>,
    pub run_id: String,
}

impl RunContext {
    /// Event `seq` of this run from `d`, at `now_ms` unless the draft
    /// carries its own time. The payload goes as given: the sink redacts and
    /// bounds it before the write.
    pub(crate) fn stamp(&self, seq: u64, now_ms: i64, d: EventDraft) -> ExecutionEvent {
        let correlation_id = d
            .correlation_id
            .or_else(|| d.session_id.clone())
            .or_else(|| self.runtime_id.clone())
            .unwrap_or_else(|| self.run_id.clone());
        ExecutionEvent {
            schema_version: TRACE_SCHEMA_VERSION,
            event_id: event_id(&self.run_id, seq),
            seq,
            ts_ms: d.ts_ms.unwrap_or(now_ms),
            sandbox: self.sandbox.clone(),
            config_hash: self.config_hash.clone(),
            runtime_id: self.runtime_id.clone(),
            run_id: self.run_id.clone(),
            session_id: d.session_id,
            correlation_id,
            parent_event_id: d.parent_event_id,
            call_id: d.call_id,
            component: d.component,
            kind: d.kind,
            node_id: d.node_id,
            status: d.status,
            duration_ms: d.duration_ms,
            payload: d.payload,
            artifact: d.artifact,
        }
    }
}

fn json_len(v: &Value) -> usize {
    serde_json::to_string(v).map_or(0, |s| s.len())
}

/// `v` within `max_bytes` serialized, and its original size when it was
/// not. Never cuts a string, an id or a number: an object loses whole
/// top-level fields, largest first (ties by name), and gains `_dropped`
/// (their names, sorted) + `_bytes` (the original size); any other value
/// over the bound becomes `{"_dropped": ["payload"], "_bytes": n}`.
pub(crate) fn bound_payload(v: Value, max_bytes: usize) -> (Value, Option<usize>) {
    let size = json_len(&v);
    if size <= max_bytes {
        return (v, None);
    }
    let Value::Object(mut o) = v else {
        return (json!({"_dropped": ["payload"], "_bytes": size}), Some(size));
    };
    let mut by_size: Vec<(usize, String)> =
        o.iter().map(|(k, x)| (json_len(x), k.clone())).collect();
    by_size.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut dropped: BTreeSet<String> = BTreeSet::new();
    let marked = |o: &serde_json::Map<String, Value>, dropped: &BTreeSet<String>| {
        let mut m = o.clone();
        m.insert("_dropped".into(), json!(dropped));
        m.insert("_bytes".into(), json!(size));
        Value::Object(m)
    };
    for (_, k) in by_size {
        if !dropped.is_empty() && json_len(&marked(&o, &dropped)) <= max_bytes {
            break;
        }
        o.remove(&k);
        dropped.insert(k);
    }
    (marked(&o, &dropped), Some(size))
}

/// Events in replay order: by run, then `seq` (never `ts_ms`: a replay
/// draft may carry an older clock); one copy of each `event_id` (a reader
/// that reconnects may get one twice).
pub(crate) fn order_events(events: &mut Vec<ExecutionEvent>) {
    events.sort_by(|a, b| (&a.run_id, a.seq).cmp(&(&b.run_id, b.seq)));
    events.dedup_by(|a, b| a.event_id == b.event_id);
}

/// One run at a glance (`tengu trace runs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RunSummary {
    pub run_id: String,
    pub sandbox: String,
    /// `run.opened` payload `kind` (`run` / `decide` / `studio`).
    pub kind: Option<String>,
    pub runtime_id: Option<String>,
    pub config_hash: Option<String>,
    pub started_ms: i64,
    pub last_ms: i64,
    pub events: u64,
    pub last_seq: u64,
    pub last_kind: String,
    pub last_status: Status,
}

impl RunSummary {
    /// The summary of one run's events (any order); `None` when empty.
    pub(crate) fn of(events: &[ExecutionEvent]) -> Option<Self> {
        let first = events.iter().min_by_key(|e| e.seq)?;
        let last = events.iter().max_by_key(|e| e.seq)?;
        let kind = events
            .iter()
            .find(|e| e.kind == RUN_OPENED)
            .and_then(|e| e.payload.get("kind"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Some(Self {
            run_id: first.run_id.clone(),
            sandbox: first.sandbox.clone(),
            kind,
            runtime_id: first.runtime_id.clone(),
            config_hash: first.config_hash.clone(),
            started_ms: first.ts_ms,
            last_ms: last.ts_ms,
            events: events.len() as u64,
            last_seq: last.seq,
            last_kind: last.kind.clone(),
            last_status: last.status,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RunContext {
        RunContext {
            sandbox: "control-loop-lab".into(),
            config_hash: Some("a".repeat(64)),
            runtime_id: None,
            run_id: "5b0c7d0e-8a4e-4f0a-9d8e-2f1c3b4a5d6e".into(),
        }
    }

    /// An oversize payload loses its biggest fields whole; ids, numbers and
    /// small strings stay intact; the result fits and says what went.
    #[test]
    fn bound_payload_drops_fields_never_cuts_ids() {
        let id = "demo:decide-demo-5b0c7d0e-8a4e-4f0a-9d8e-2f1c3b4a5d6e:1";
        let decision = "gen-1759912345-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";
        let v = json!({
            "call_id": id,
            "decision_id": decision,
            "latency_ms": 412,
            "answers": {"next_action": {"value": "x".repeat(3000), "confidence": 0.9}},
            "output": "y".repeat(2000),
        });
        let (out, was) = bound_payload(v.clone(), 1024);
        assert!(was.unwrap() > 1024);
        assert!(json_len(&out) <= 1024, "{}", json_len(&out));
        assert_eq!(out["call_id"], json!(id));
        assert_eq!(out["decision_id"], json!(decision));
        assert_eq!(out["latency_ms"], json!(412));
        assert_eq!(out["_dropped"], json!(["answers", "output"]));
        assert_eq!(out["_bytes"], json!(was.unwrap()));
        // Only the largest goes when that is enough.
        let (out, _) = bound_payload(v.clone(), 2500);
        assert_eq!(out["_dropped"], json!(["answers"]));
        assert_eq!(out["output"], v["output"]);
        // Within the bound: untouched, no marker.
        assert_eq!(bound_payload(v.clone(), 1 << 20), (v, None));
        // A non-object over the bound is replaced, never cut.
        let (s, n) = bound_payload(json!("z".repeat(100)), 10);
        assert_eq!(s, json!({"_dropped": ["payload"], "_bytes": n.unwrap()}));
    }

    /// Replay order is the run's `seq`, whatever the clock or arrival order;
    /// a twice-received event appears once.
    #[test]
    fn ordering_by_seq() {
        let c = ctx();
        let mk = |seq: u64, ts: i64| {
            c.stamp(
                seq,
                0,
                EventDraft::new(Component::Loop, "loop.started", Status::Running).at(ts),
            )
        };
        let mut evs = vec![mk(3, 100), mk(1, 300), mk(2, 200), mk(3, 100), mk(4, 50)];
        order_events(&mut evs);
        let seqs: Vec<u64> = evs.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [1, 2, 3, 4]);
        assert_eq!(evs[3].ts_ms, 50, "ts_ms never reorders");
        let s = RunSummary::of(&evs).unwrap();
        assert_eq!((s.events, s.last_seq, s.started_ms), (4, 4, 300));
    }

    /// The correlation rule and the envelope: every field present (null
    /// when unknown), ids in full; unknown fields in a newer line are ignored.
    #[test]
    fn envelope_round_trips_and_tolerates_unknown_fields() {
        let mut c = ctx();
        let lifecycle = c.stamp(
            1,
            7,
            EventDraft::new(Component::Runtime, RUN_OPENED, Status::Ok),
        );
        assert_eq!(lifecycle.correlation_id, c.run_id);
        assert_eq!(lifecycle.event_id, format!("{}:1", c.run_id));
        c.runtime_id = Some("host:42:0f0e0d0c-0b0a-4908-8706-050403020100".into());
        let e = c.stamp(
            2,
            8,
            EventDraft::new(Component::Tool, "tool.completed", Status::Ok)
                .session("tick:1759912340000")
                .call("demo:tick:1759912340000:1")
                .node("tool:lab/write_file")
                .duration(3),
        );
        assert_eq!(e.correlation_id, "tick:1759912340000");
        let v = serde_json::to_value(&e).unwrap();
        for k in [
            "schema_version",
            "event_id",
            "seq",
            "ts_ms",
            "sandbox",
            "config_hash",
            "runtime_id",
            "run_id",
            "session_id",
            "correlation_id",
            "parent_event_id",
            "call_id",
            "component",
            "kind",
            "node_id",
            "status",
            "duration_ms",
            "payload",
            "artifact",
        ] {
            assert!(v.get(k).is_some(), "{k}: {v}");
        }
        assert_eq!(v["component"], json!("tool"));
        assert_eq!(v["status"], json!("ok"));
        let lifecycle_only = c.stamp(3, 9, EventDraft::new(Component::Runtime, "x", Status::Ok));
        assert_eq!(lifecycle_only.correlation_id, c.runtime_id.clone().unwrap());
        let feed = c.stamp(
            4,
            10,
            EventDraft::new(Component::Feed, "feed.failed", Status::Failed)
                .session("probe:1759912350000")
                .correlation("feed:probe:1759912350000")
                .parent(e.event_id.clone())
                .artifact(
                    "logs/decisions.jsonl",
                    Some("demo:tick:1759912340000:1".into()),
                ),
        );
        assert_eq!(
            feed.correlation_id, "feed:probe:1759912350000",
            "the draft's wins"
        );
        assert_eq!(feed.parent_event_id.as_deref(), Some(e.event_id.as_str()));
        assert_eq!(
            feed.artifact.unwrap().key.as_deref(),
            Some("demo:tick:1759912340000:1")
        );
        let mut newer = v.clone();
        newer["a_future_field"] = json!(1);
        let back: ExecutionEvent = serde_json::from_value(newer).unwrap();
        assert_eq!(back, e);
    }
}
