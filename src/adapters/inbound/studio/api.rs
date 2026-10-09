//! Studio JSON routes (`studio/mod.rs` route table). Every value comes from
//! Rust that already decided it — the validated graph, the trace store, the
//! `tengu doctor --live` verdict; nothing here re-derives a rule. Errors are
//! `{"error": …}` with a status: 400 bad input, 404 unknown, 422 a kept map
//! this config refuses, 500 a store that cannot be read.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use super::AppState;
use crate::adapters::outbound::trace_store::check_run_id;
use crate::application::studio::stream::pick_live;
use crate::bootstrap::studio::MapGraphError;
use crate::domain::observation::now_ms;
use crate::domain::runtime::HeartbeatRead;
use crate::domain::trace::{scrub_value, TRACE_SCHEMA_VERSION};
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
        "read_only": true,
        "control_enabled": false,
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
    }))
    .into_response()
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
        .map(|c| json!({"subject": c.subject, "ok": c.ok, "detail": c.detail}))
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

#[derive(Deserialize)]
pub(super) struct EventsQuery {
    after: Option<u64>,
    limit: Option<usize>,
}

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
    let (st2, id) = (Arc::clone(&st), run_id.clone());
    let read =
        tokio::task::spawn_blocking(move || st2.ctx.reader.events(&id, after, limit + 1)).await;
    let mut evs = match read {
        Ok(Ok(evs)) => evs,
        Ok(Err(e)) => return read_error(&e),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let more = evs.len() > limit;
    evs.truncate(limit);
    let next_after = evs.last().map_or(after, |e| e.seq);
    Json(json!({
        "run_id": run_id,
        "after": after,
        "next_after": next_after,
        "more": more,
        "events": evs,
    }))
    .into_response()
}
