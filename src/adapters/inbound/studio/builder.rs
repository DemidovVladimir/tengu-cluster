//! Studio builder routes (`tengu studio --sandbox <s> --allow-edit`;
//! `docs/studio-builder-2026-10-10.md`). Every verdict comes from
//! `application::builder` (compile + the real config loader); this file
//! parses the request, runs the use case off the async workers and maps its
//! refusal to a status. Without `--allow-edit` every route here is 404.
//!
//! | Route | Use case | Status |
//! |---|---|---|
//! | GET `/builder` | the page (`web/studio/builder.html`) | 200 · 404 without `--allow-edit` |
//! | GET `/api/v1/builder` | `Builder::state` | 200 |
//! | PUT `/api/v1/builder/blueprint` | `Builder::save` → `Status` | 200 · 400 · 403 view-only |
//! | POST `/api/v1/builder/preview` | `Builder::preview` | 200 · 400 |
//! | POST `/api/v1/builder/finalise` `{blueprint, sha256}` | `Builder::finalise` | 200 · 400 · 403 · 409 stale preview / runtime running · 422 refused |
//!
//! Bodies: `Content-Type: application/json` (else 415), unknown fields 400
//! (the blueprint is `deny_unknown_fields`), ≤ 1 MiB (the router's limit).
//! Every request logs one `builder:` line (route, sandbox, outcome, ms).

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;

use super::api::error;
use super::AppState;
use crate::application::builder::{Builder, BuilderError};
use crate::domain::blueprint::Blueprint;

type St = State<Arc<AppState>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinaliseBody {
    blueprint: Blueprint,
    sha256: String,
}

fn builder(st: &AppState) -> Result<Arc<Builder>, Response> {
    st.builder.clone().ok_or_else(|| {
        error(
            StatusCode::NOT_FOUND,
            "the builder is off: start `tengu studio --sandbox <name> --allow-edit` (or `tengu sandbox new`)",
        )
    })
}

fn json_body<T: serde::de::DeserializeOwned>(headers: &HeaderMap, body: &Bytes) -> Result<T, Response> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"));
    if !json {
        return Err(error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "a builder body is JSON: Content-Type: application/json",
        ));
    }
    serde_json::from_slice(body).map_err(|e| error(StatusCode::BAD_REQUEST, format!("body: {e}")))
}

fn refusal(e: BuilderError) -> Response {
    let code = match e {
        BuilderError::BadRequest(_) => StatusCode::BAD_REQUEST,
        BuilderError::Forbidden(_) => StatusCode::FORBIDDEN,
        BuilderError::Conflict(_) => StatusCode::CONFLICT,
        BuilderError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
        BuilderError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error(code, e.to_string())
}

/// Run `f` on the blocking pool (it reads and writes files, runs the
/// loader), log one line, answer JSON.
async fn run<T, F>(route: &'static str, b: Arc<Builder>, f: F) -> Response
where
    T: serde::Serialize + Send + 'static,
    F: FnOnce(&Builder) -> Result<T, BuilderError> + Send + 'static,
{
    let started = Instant::now();
    let sandbox = b.sandbox().to_string();
    let out = tokio::task::spawn_blocking(move || f(&b)).await;
    let ms = started.elapsed().as_millis();
    match out {
        Ok(Ok(v)) => {
            tracing::info!(route, sandbox = %sandbox, ms, "builder: ok");
            Json(v).into_response()
        }
        Ok(Err(e)) => {
            tracing::warn!(route, sandbox = %sandbox, ms, error = %e, "builder: refused");
            refusal(e)
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// GET `/builder`.
pub(super) async fn page(State(st): St) -> Response {
    match builder(&st) {
        Ok(_) => super::assets::builder_page(),
        Err(r) => r,
    }
}

/// GET `/api/v1/builder`.
pub(super) async fn state(State(st): St) -> Response {
    match builder(&st) {
        Ok(b) => run("state", b, |b| b.state()).await,
        Err(r) => r,
    }
}

/// PUT `/api/v1/builder/blueprint`.
pub(super) async fn save(State(st): St, headers: HeaderMap, body: Bytes) -> Response {
    let b = match builder(&st) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let bp: Blueprint = match json_body(&headers, &body) {
        Ok(bp) => bp,
        Err(r) => return r,
    };
    run("save", b, move |b| b.save(&bp)).await
}

/// POST `/api/v1/builder/preview`.
pub(super) async fn preview(State(st): St, headers: HeaderMap, body: Bytes) -> Response {
    let b = match builder(&st) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let bp: Blueprint = match json_body(&headers, &body) {
        Ok(bp) => bp,
        Err(r) => return r,
    };
    run("preview", b, move |b| b.preview(&bp)).await
}

/// POST `/api/v1/builder/finalise`.
pub(super) async fn finalise(State(st): St, headers: HeaderMap, body: Bytes) -> Response {
    let b = match builder(&st) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let fb: FinaliseBody = match json_body(&headers, &body) {
        Ok(fb) => fb,
        Err(r) => return r,
    };
    run("finalise", b, move |b| b.finalise(&fb.blueprint, &fb.sha256)).await
}
