//! The Studio pages (`web/studio/`), compiled into the binary with
//! `include_str!`: no file is read at run time, nothing loads from the
//! network. A fixed table — no path from a request reaches the disk.

use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

const INDEX: &str = include_str!("../../../../web/studio/index.html");
/// The builder page (`/builder`, served only with `--allow-edit`).
const BUILDER: &str = include_str!("../../../../web/studio/builder.html");

/// `/assets/<name>` → (content type, body).
const ASSETS: &[(&str, &str, &str)] = &[
    (
        "studio.css",
        "text/css; charset=utf-8",
        include_str!("../../../../web/studio/studio.css"),
    ),
    (
        "studio.js",
        "text/javascript; charset=utf-8",
        include_str!("../../../../web/studio/studio.js"),
    ),
    (
        "builder.css",
        "text/css; charset=utf-8",
        include_str!("../../../../web/studio/builder.css"),
    ),
    (
        "builder.js",
        "text/javascript; charset=utf-8",
        include_str!("../../../../web/studio/builder.js"),
    ),
];

pub(super) async fn index() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX).into_response()
}

pub(super) fn builder_page() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        BUILDER,
    )
        .into_response()
}

/// No icon: 204, so a browser's automatic request logs no error.
pub(super) async fn favicon() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

pub(super) async fn asset(Path(name): Path<String>) -> Response {
    match ASSETS.iter().find(|(n, ..)| *n == name) {
        Some((_, kind, body)) => ([(header::CONTENT_TYPE, *kind)], *body).into_response(),
        None => (StatusCode::NOT_FOUND, "no such asset").into_response(),
    }
}
