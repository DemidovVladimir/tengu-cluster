//! Studio JSON routes (`studio/mod.rs` route table). Every value comes from
//! Rust that already decided it — the validated graph, the trace store, the
//! `tengu doctor --live` verdict, the board fold (`application::studio::board`:
//! colours, edges, grey), the inspector slice (`application::studio::inspect`);
//! nothing here re-derives a rule; the control routes hand a parsed request
//! to `control::Controller` (rules `application::studio::control`). Errors
//! are `{"error": …}` with a status: 400 bad input, 404 unknown, 415 a body
//! that is not JSON, 422 a kept map this config refuses, 500 a store that
//! cannot be read; a control verdict is `{ok, status, detail, …}` with its
//! own status (`studio/mod.rs` route table).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::control::Verdict;
use super::AppState;
use crate::adapters::outbound::trace_store::check_run_id;
use crate::application::studio::board::{fold_run, EventView};
use crate::application::studio::stream::pick_live;
use crate::bootstrap::studio::{MapGraphError, NodeError};
use crate::domain::observation::now_ms;
use crate::domain::runtime::HeartbeatRead;
use crate::domain::trace::{
    scrub_value, ExecutionEvent, RunKind, RunState, RunSummary, Status, Tone, TRACE_SCHEMA_VERSION,
};
use crate::domain::workflow::WORKFLOW_SCHEMA_VERSION;

/// Version of this API's JSON shapes (`/api/v1/…`).
pub(crate) const API_VERSION: u32 = 1;
/// `/events` page size: default · largest.
pub(crate) const PAGE_DEFAULT: usize = 500;
pub(crate) const PAGE_MAX: usize = 1000;

type St = State<Arc<AppState>>;

pub(super) fn error(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"error": msg.into()}))).into_response()
}

/// An `Err` of a trace read: 404 when the run's file does not exist.
pub(super) fn read_error(e: &anyhow::Error) -> Response {
    let missing = e
        .chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|io| io.kind() == std::io::ErrorKind::NotFound);
    if missing {
        error(StatusCode::NOT_FOUND, "no such run")
    } else {
        error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

pub(super) async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "no such route")
}

pub(super) async fn meta(State(st): St) -> Response {
    Json(json!({
        "api_version": API_VERSION,
        "sandbox": st.ctx.sandbox,
        "config_hash": st.ctx.graph.config_hash,
        "schema": {"workflow": WORKFLOW_SCHEMA_VERSION, "trace": TRACE_SCHEMA_VERSION},
        "read_only": !st.control.enabled(),
        "control_enabled": st.control.enabled(),
        "server": {
            "pid": std::process::id(),
            "started_ms": st.started_ms,
            "version": env!("CARGO_PKG_VERSION"),
        },
        "limits": {
            "page_default": PAGE_DEFAULT,
            "page_max": PAGE_MAX,
            "client_buffer": st.limits.client_buffer,
        },
        "evidence": st.ctx.evidence(),
        "tones": tones(),
    }))
    .into_response()
}

/// The colour legend as Rust decides it (`Status::tone`, `Tone`): the page
/// looks a status up here and draws that tone's token.
fn tones() -> Value {
    let statuses: Vec<Value> = Status::ALL
        .iter()
        .map(|s| json!({"status": s, "tone": s.tone(), "meaning": s.meaning()}))
        .collect();
    let tones: Vec<Value> = Tone::ALL
        .iter()
        .map(|t| json!({"tone": t, "meaning": t.meaning()}))
        .collect();
    json!({"statuses": statuses, "tones": tones})
}

#[derive(Deserialize)]
pub(super) struct GraphQuery {
    map: Option<String>,
}

pub(super) async fn graph(State(st): St, Query(q): Query<GraphQuery>) -> Response {
    let Some(sha) = q.map else {
        return Json(&st.ctx.graph).into_response();
    };
    let st2 = Arc::clone(&st);
    let built = tokio::task::spawn_blocking(move || st2.ctx.map_graph(&sha)).await;
    match built {
        Ok(Ok(g)) => Json(g).into_response(),
        Ok(Err(MapGraphError::BadId)) => {
            error(StatusCode::BAD_REQUEST, "map: a sha256 (64 lowercase hex)")
        }
        Ok(Err(MapGraphError::Missing)) => error(
            StatusCode::NOT_FOUND,
            "no kept map with that sha256 (`tengu decide --map` keeps each one it runs)",
        ),
        Ok(Err(MapGraphError::Unreadable(why))) => {
            error(StatusCode::UNPROCESSABLE_ENTITY, format!("kept map: {why}"))
        }
        Ok(Err(MapGraphError::Refused(reasons))) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "execution map refused by this config", "reasons": reasons})),
        )
            .into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// A `doctor --live` verdict as a tone: ok = green, else red.
fn verdict_tone(ok: bool) -> Tone {
    if ok {
        Tone::Green
    } else {
        Tone::Red
    }
}

pub(super) async fn health(State(st): St) -> Response {
    let live = st.ctx.health().await;
    let (read, value, err) = match &live.heartbeat {
        HeartbeatRead::Found(hb) => ("found", serde_json::to_value(hb).ok(), None),
        HeartbeatRead::Missing => ("missing", None, None),
        HeartbeatRead::Unreadable(e) => ("unreadable", None, Some(e.clone())),
    };
    let checks: Vec<Value> = live
        .report
        .checks
        .iter()
        .map(|c| json!({"subject": c.subject, "ok": c.ok, "tone": verdict_tone(c.ok), "detail": c.detail}))
        .collect();
    let mut v = json!({
        "sandbox": live.sandbox,
        "at_ms": now_ms(),
        "heartbeat": {
            "read": read,
            "file": st.ctx.evidence().heartbeat,
            "value": value,
            "error": err,
        },
        "live": live.report.ok(),
        "tone": verdict_tone(live.report.ok()),
        "checks": checks,
    });
    scrub_value(&mut v, &st.ctx.secrets);
    Json(v).into_response()
}

pub(super) async fn runs(State(st): St) -> Response {
    let st2 = Arc::clone(&st);
    let read = tokio::task::spawn_blocking(move || {
        let holder = (st2.ctx.holder_fn())();
        st2.ctx.reader.runs().map(|r| (r, holder))
    })
    .await;
    let (runs, holder) = match read {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => return read_error(&e),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let live = holder
        .as_deref()
        .and_then(|h| pick_live(&runs, h))
        .map(|r| r.run_id.clone());
    let current = st.ctx.graph.config_hash.clone();
    let rows: Vec<Value> = runs
        .iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
            if let Value::Object(o) = &mut v {
                o.insert(
                    "config_current".into(),
                    json!(current.is_some() && r.config_hash == current),
                );
                o.insert("state".into(), json!(RunState::of(r, live.as_deref())));
            }
            v
        })
        .collect();
    Json(json!({
        "sandbox": st.ctx.sandbox,
        "config_hash": current,
        "holder": holder,
        "live_run_id": live,
        "runs": rows,
    }))
    .into_response()
}

/// Every event of `run_id` (`seq` order), read off the async workers.
async fn read_run(st: &Arc<AppState>, run_id: &str) -> Result<Vec<ExecutionEvent>, Response> {
    if let Err(e) = check_run_id(run_id) {
        return Err(error(StatusCode::BAD_REQUEST, format!("{e:#}")));
    }
    let (st2, id) = (Arc::clone(st), run_id.to_string());
    match tokio::task::spawn_blocking(move || st2.ctx.reader.events(&id, 0, usize::MAX)).await {
        Ok(Ok(evs)) => Ok(evs),
        Ok(Err(e)) => Err(read_error(&e)),
        Err(e) => Err(error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// An `/events` page.
#[derive(Serialize)]
struct Page<'a> {
    run_id: &'a str,
    after: u64,
    next_after: u64,
    more: bool,
    graph: Value,
    events: Vec<WithView<'a>>,
}

/// One event as written, plus its board view.
#[derive(Serialize)]
struct WithView<'a> {
    #[serde(flatten)]
    event: &'a ExecutionEvent,
    view: EventView,
}

#[derive(Deserialize)]
pub(super) struct EventsQuery {
    after: Option<u64>,
    limit: Option<usize>,
}

/// A page of a run's events, each with its `view` (`board::EventView`:
/// tone, facets, highlighted edges — folded from `seq` 1 over the graph the
/// run is drawn on, so any page carries the views a full read would).
pub(super) async fn events(
    State(st): St,
    Path(run_id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Response {
    if let Err(e) = check_run_id(&run_id) {
        return error(StatusCode::BAD_REQUEST, format!("{e:#}"));
    }
    let limit = q.limit.unwrap_or(PAGE_DEFAULT);
    if !(1..=PAGE_MAX).contains(&limit) {
        return error(
            StatusCode::BAD_REQUEST,
            format!("limit: 1..={PAGE_MAX} (default {PAGE_DEFAULT})"),
        );
    }
    let after = q.after.unwrap_or(0);
    let evs = match read_run(&st, &run_id).await {
        Ok(evs) => evs,
        Err(r) => return r,
    };
    let rg = st.ctx.run_graph(&evs);
    let end = evs.iter().filter(|e| e.seq <= after).count() + limit;
    let cut = &evs[..end.min(evs.len())];
    let (views, _) = fold_run(&rg.graph, cut, None);
    let events: Vec<WithView> = cut
        .iter()
        .zip(views)
        .filter(|(e, _)| e.seq > after)
        .map(|(event, view)| WithView { event, view })
        .collect();
    let more = evs.len() > end;
    let next_after = events.last().map_or(after, |e| e.event.seq);
    // A struct, not `json!`: each event keeps the field order of its trace
    // line (`tengu trace show`), `view` last.
    Json(Page {
        run_id: &run_id,
        after,
        next_after,
        more,
        graph: json!({"map": rg.map, "note": rg.note}),
        events,
    })
    .into_response()
}

#[derive(Deserialize)]
pub(super) struct BoardQuery {
    upto: Option<u64>,
}

/// `/api/v1/runs/:run_id/board?upto=<seq>`: the run folded over its graph
/// up to `seq` (all when absent) — node colours, highlighted edges, grey
/// legal sets, header facts (`application::studio::board`) — plus the run's
/// state and trace file.
pub(super) async fn board(
    State(st): St,
    Path(run_id): Path<String>,
    Query(q): Query<BoardQuery>,
) -> Response {
    let evs = match read_run(&st, &run_id).await {
        Ok(evs) => evs,
        Err(r) => return r,
    };
    let Some(summary) = RunSummary::of(&evs) else {
        return error(StatusCode::NOT_FOUND, "no events in this run");
    };
    let holder = (st.ctx.holder_fn())();
    let holders = summary.kind.as_deref() == Some(RunKind::Run.as_str())
        && summary.runtime_id.is_some()
        && summary.runtime_id == holder;
    let state = RunState::of(&summary, holders.then_some(summary.run_id.as_str()));
    let rg = st.ctx.run_graph(&evs);
    let (_, board) = fold_run(&rg.graph, &evs, q.upto);
    let current = st.ctx.graph.config_hash.clone();
    Json(json!({
        "run_id": run_id,
        "last_seq": summary.last_seq,
        "run": {
            "summary": summary,
            "state": state,
            "config_current": current.is_some() && summary.config_hash == current,
            "trace_file": st.ctx.run_file(&run_id),
        },
        "graph": {"map": rg.map, "note": rg.note},
        "board": board,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub(super) struct NodeQuery {
    map: Option<String>,
}

/// `/api/v1/nodes/:node_id[?map=<sha256>]`: the inspector's node — the
/// graph node, the validated config section behind it, its edges and
/// evidence files (`StudioContext::node_detail`); never the TOML text.
pub(super) async fn node(
    State(st): St,
    Path(node_id): Path<String>,
    Query(q): Query<NodeQuery>,
) -> Response {
    let st2 = Arc::clone(&st);
    let found =
        tokio::task::spawn_blocking(move || st2.ctx.node_detail(&node_id, q.map.as_deref())).await;
    match found {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(NodeError::Missing)) => error(StatusCode::NOT_FOUND, "no such node in this graph"),
        Ok(Err(NodeError::Map(MapGraphError::BadId))) => {
            error(StatusCode::BAD_REQUEST, "map: a sha256 (64 lowercase hex)")
        }
        Ok(Err(NodeError::Map(MapGraphError::Missing))) => {
            error(StatusCode::NOT_FOUND, "no kept map with that sha256")
        }
        Ok(Err(NodeError::Map(e))) => {
            error(StatusCode::UNPROCESSABLE_ENTITY, format!("kept map: {e}"))
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `GET /api/v1/control` (`Controller::view`), redacted.
pub(super) async fn control(State(st): St) -> Response {
    let mut v = st.control.view();
    scrub_value(&mut v, &st.ctx.secrets);
    Json(v).into_response()
}

/// A control body: empty = every field absent; else JSON
/// (`Content-Type: application/json`, unknown fields refused).
fn control_body<T: serde::de::DeserializeOwned + Default>(
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<T, (StatusCode, String)> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"));
    if !json {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "a control body is JSON: Content-Type: application/json".into(),
        ));
    }
    serde_json::from_slice(body).map_err(|e| (StatusCode::BAD_REQUEST, format!("body: {e}")))
}

/// A verdict as HTTP: its status; `{ok, action, status, detail, at_ms,
/// event_id, …, extra fields, control: the view after it}`, redacted.
fn answer(st: &AppState, v: Verdict) -> Response {
    let mut body = json!({
        "ok": v.outcome.status == Status::Ok,
        "outcome": v.outcome,
    });
    if let (Value::Object(o), Value::Object(x)) = (&mut body, v.extra) {
        o.extend(x);
    }
    body["control"] = st.control.view();
    scrub_value(&mut body, &st.ctx.secrets);
    (v.code, Json(body)).into_response()
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PlayBody {
    /// Also send this scenario once running.
    scenario: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StopBody {}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EventBody {
    scenario: Option<String>,
    /// The loop (else the only one).
    #[serde(rename = "loop")]
    loop_name: Option<String>,
}

/// `POST /api/v1/control/play {scenario?}`.
pub(super) async fn play(State(st): St, headers: HeaderMap, body: Bytes) -> Response {
    let b: PlayBody = match control_body(&headers, &body) {
        Ok(b) => b,
        Err((code, why)) => return error(code, why),
    };
    let v = st.control.play(b.scenario).await;
    answer(&st, v)
}

/// `POST /api/v1/control/stop`.
pub(super) async fn stop(State(st): St, headers: HeaderMap, body: Bytes) -> Response {
    if let Err((code, why)) = control_body::<StopBody>(&headers, &body) {
        return error(code, why);
    }
    let v = st.control.stop().await;
    answer(&st, v)
}

/// `POST /api/v1/control/event {scenario, loop?}`.
pub(super) async fn event(State(st): St, headers: HeaderMap, body: Bytes) -> Response {
    let b: EventBody = match control_body(&headers, &body) {
        Ok(b) => b,
        Err((code, why)) => return error(code, why),
    };
    let Some(scenario) = b.scenario else {
        return error(
            StatusCode::BAD_REQUEST,
            "body: {\"scenario\": \"<name>\"} (GET /api/v1/control lists them)",
        );
    };
    let v = st.control.event(&scenario, b.loop_name.as_deref()).await;
    answer(&st, v)
}
