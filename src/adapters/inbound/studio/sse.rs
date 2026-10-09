//! Studio SSE routes (`studio/mod.rs` § SSE): `application::studio::stream`
//! items as server-sent events. Delivery rules (backlog, lag, live run) are
//! the use case's; here only the wire format, the resume point and the
//! stream cap.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc::Receiver;
use tokio::sync::OwnedSemaphorePermit;
use tokio_stream::wrappers::ReceiverStream;

use super::api::{error, read_error};
use super::AppState;
use crate::adapters::outbound::trace_store::check_run_id;
use crate::application::studio::stream::{follow_live, follow_run, StreamItem};
use crate::domain::trace::parse_event_id;

/// SSE comment period (keeps proxies and the browser from timing out).
const KEEP_ALIVE: Duration = Duration::from_secs(15);

type St = State<Arc<AppState>>;

#[derive(Deserialize)]
pub(super) struct StreamQuery {
    after: Option<u64>,
}

/// The browser's `Last-Event-ID` (an `event_id`), split.
fn last_event_id(headers: &HeaderMap) -> Option<(String, u64)> {
    let v = headers.get("last-event-id")?.to_str().ok()?;
    parse_event_id(v).map(|(r, s)| (r.to_string(), s))
}

/// A stream slot ([`super::MAX_STREAMS`]); `None` when all are taken.
fn permit(st: &AppState) -> Option<OwnedSemaphorePermit> {
    Arc::clone(&st.streams).try_acquire_owned().ok()
}

fn busy() -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "too many open streams; close a Studio tab",
    )
}

/// `/api/v1/runs/:run_id/stream`: after `Last-Event-ID` (wins: the
/// browser's reconnect), else `?after=`, else from `seq` 1.
pub(super) async fn run_stream(
    State(st): St,
    Path(run_id): Path<String>,
    Query(q): Query<StreamQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = check_run_id(&run_id) {
        return error(StatusCode::BAD_REQUEST, format!("{e:#}"));
    }
    let after = match last_event_id(&headers) {
        Some((r, seq)) if r == run_id => seq,
        Some((r, _)) => {
            return error(
                StatusCode::BAD_REQUEST,
                format!("Last-Event-ID names run {r}, not {run_id}"),
            )
        }
        None => q.after.unwrap_or(0),
    };
    let Some(permit) = permit(&st) else {
        return busy();
    };
    // The file reads (`last_seq`) off the async workers.
    let (st2, id) = (Arc::clone(&st), run_id.clone());
    let opened =
        tokio::task::spawn_blocking(move || follow_run(&*st2.ctx.reader, &id, after, st2.limits))
            .await;
    match opened {
        Ok(Ok(rx)) => sse(rx, permit, &st),
        Ok(Err(e)) => read_error(&e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `/api/v1/live/stream`: the lease holder's run (`event: run` first and
/// on every restart); `Last-Event-ID` resumes inside the live run.
pub(super) async fn live_stream(State(st): St, headers: HeaderMap) -> Response {
    let Some(permit) = permit(&st) else {
        return busy();
    };
    let rx = follow_live(
        Arc::clone(&st.ctx.reader),
        st.ctx.holder_fn(),
        last_event_id(&headers),
        st.limits,
    );
    sse(rx, permit, &st)
}

fn sse(rx: Receiver<StreamItem>, permit: OwnedSemaphorePermit, st: &AppState) -> Response {
    let mut stop = st.shutdown.clone();
    let stopped = async move {
        let _ = stop.wait_for(|s| *s).await;
    };
    let events = ReceiverStream::new(rx)
        .map(move |item| {
            // The permit lives as long as the stream.
            let _held = &permit;
            Ok::<_, Infallible>(event(item))
        })
        .take_until(stopped);
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

/// One item on the wire: `trace` (id = `event_id`), `run`, `lagged` (id =
/// the last event handed, so a reconnect resumes after it).
fn event(item: StreamItem) -> Event {
    let built = match &item {
        StreamItem::Trace(ev) => Event::default()
            .id(ev.event_id.clone())
            .event("trace")
            .json_data(ev),
        StreamItem::Run(a) => Event::default().event("run").json_data(a),
        StreamItem::Lagged(l) => {
            let e = Event::default().event("lagged");
            let e = match &l.last_event_id {
                Some(id) => e.id(id.clone()),
                None => e,
            };
            e.json_data(l)
        }
    };
    built.unwrap_or_else(|e| Event::default().event("error").data(e.to_string()))
}
