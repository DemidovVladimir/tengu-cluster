//! A2A server — `tengu a2a serve --sandbox <s>` (feature `a2a`, on by
//! default): serves this sandbox's planner and / or agents to other agent
//! harnesses over A2A (JSON-RPC 1.0 and 0.3, SSE streaming). The protocol
//! lives in `application/a2a/`, the runner and cards in `bootstrap/a2a.rs`,
//! the section in `config/a2a.rs`. Doc: `docs/a2a-2026-10-10.md`.
//!
//! | Route | Answer |
//! |---|---|
//! | `GET /.well-known/agent-card.json` | the root card: the planner's (`orchestrator = true`), else the first agent's |
//! | `GET /a2a/.well-known/agent-card.json` · `GET /a2a/agents/<name>/.well-known/agent-card.json` | the planner's / that agent's card |
//! | `GET …/.well-known/agent.json` (each of the above) | the same card in 0.3 form (`v03::card_to_v03`); also `A2A-Version: 0.3` or `?A2A-Version=0.3` on the `agent-card.json` path |
//! | `POST /a2a` · `POST /a2a/agents/<name>` | JSON-RPC (`application/a2a/` method table): a JSON response, or SSE (`text/event-stream`) for the streaming methods |
//!
//! | Concern | Rule |
//! |---|---|
//! | Auth | `token_env` set ⇒ every `POST` needs `Authorization: Bearer <$token_env>` (constant-time compare, read once at start; an unset or empty variable stops the start); else 401 + `WWW-Authenticate: Bearer`. Cards are public (spec § 8.2) |
//! | Unknown agent | 404 |
//! | Body | ≤ 1 MiB |
//! | Cards | `Cache-Control: public, max-age=300` + `ETag`; `If-None-Match` ⇒ 304 |
//! | Leases | none: the server runs no loop, feed or exec tool (a private agent is never served), so it may run beside `tengu run` of the same sandbox |
//! | Stop | SIGINT / SIGTERM: stop accepting, every unfinished task `FAILED` ("server stopped") |
//! | Logs | one `tengu::a2a` line per request, task start, settle (endpoint, method, task id, state); never a message body |

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::application::a2a::{stream_event, A2aService, Reply};
use crate::bootstrap::a2a::Cards;
use crate::config::Config;
use crate::domain::a2a::card::etag;
use crate::domain::a2a::model::{AgentCard, Dialect};
use crate::domain::a2a::rpc::{version_ask, VersionAsk};
use crate::domain::a2a::v03;
use crate::domain::secrets::SecretRegistry;
use crate::ports::a2a::A2aTarget;

/// Largest request body.
const MAX_BODY: usize = 1024 * 1024;

/// Per-process state of the routes.
pub(crate) struct A2aState {
    cards: Cards,
    service: A2aService,
    /// `Bearer <token>` every `POST` must carry; `None` = open (loopback).
    expected_auth: Option<String>,
}

/// Run the server until SIGINT / SIGTERM (module table).
pub async fn run_a2a_server(config: Config, secrets: Arc<SecretRegistry>) -> Result<()> {
    let server = crate::bootstrap::a2a::server_config(&config)?.clone();
    let expected_auth = match &server.token_env {
        Some(var) => {
            let token = std::env::var(var)
                .ok()
                .filter(|t| !t.trim().is_empty())
                .ok_or_else(|| {
                    anyhow!("[a2a.server] token_env = \"{var}\" but {var} is unset or empty — set it before `tengu a2a serve`")
                })?;
            if token.len() < 24 {
                warn!(target: "tengu::a2a", "{var} is shorter than 24 characters — use a long random token");
            }
            Some(format!("Bearer {}", token.trim()))
        }
        None => None,
    };
    let cards = crate::bootstrap::a2a::cards(&config)?;
    let service = crate::bootstrap::a2a::build_service(&config, secrets)?;
    let addr: SocketAddr = format!("{}:{}", server.bind, server.port)
        .parse()
        .or_else(|_| format!("[{}]:{}", server.bind, server.port).parse())
        .with_context(|| format!("invalid [a2a.server] bind {}:{}", server.bind, server.port))?;
    let endpoints: Vec<String> = cards
        .planner
        .iter()
        .map(|c| format!("planner → {}", c.supported_interfaces[0].url))
        .chain(
            cards
                .agents
                .iter()
                .map(|(n, c)| format!("{n} → {}", c.supported_interfaces[0].url)),
        )
        .collect();
    let state = Arc::new(A2aState {
        cards,
        service: service.clone(),
        expected_auth,
    });
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind the A2A server to {addr}"))?;
    let mut signals = super::run::Signals::install()?;
    info!(
        target: "tengu::a2a",
        %addr,
        base_url = %server.base_url(),
        auth = if server.token_env.is_some() { "bearer" } else { "none (loopback)" },
        endpoints = ?endpoints,
        sandbox = ?config.sandbox_name,
        "tengu a2a serve listening"
    );
    eprintln!(
        "tengu a2a serve: {} (card: {}/.well-known/agent-card.json)",
        endpoints.join(", "),
        server.base_url()
    );
    let served = axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            let sig = signals.next().await;
            info!(target: "tengu::a2a", signal = sig, "tengu a2a serve stopping");
        })
        .await
        .context("A2A server exited unexpectedly");
    service.stop_all();
    served
}

/// Every route (module table).
pub(crate) fn router(state: Arc<A2aState>) -> Router {
    Router::new()
        .route("/.well-known/agent-card.json", get(root_card))
        .route("/.well-known/agent.json", get(root_card_v03))
        .route("/a2a", post(planner_rpc))
        .route("/a2a/.well-known/agent-card.json", get(planner_card))
        .route("/a2a/.well-known/agent.json", get(planner_card_v03))
        .route("/a2a/agents/:name", post(agent_rpc))
        .route(
            "/a2a/agents/:name/.well-known/agent-card.json",
            get(agent_card),
        )
        .route(
            "/a2a/agents/:name/.well-known/agent.json",
            get(agent_card_v03),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
}

type Q = Query<std::collections::HashMap<String, String>>;

fn not_found(what: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": format!("unknown A2A endpoint: {what}")})),
    )
        .into_response()
}

/// The card in the dialect asked for (header or query `A2A-Version`;
/// `force_v03` for the `agent.json` path), with caching headers.
fn card_response(card: &AgentCard, headers: &HeaderMap, query: &Q, force_v03: bool) -> Response {
    let asked = headers
        .get("a2a-version")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            query
                .0
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("a2a-version"))
                .map(|(_, v)| v.clone())
        });
    let v03 = force_v03 || version_ask(asked.as_deref()) == VersionAsk::Speaks(Dialect::V03);
    let body = if v03 {
        let url = card
            .supported_interfaces
            .first()
            .map(|i| i.url.clone())
            .unwrap_or_default();
        v03::card_to_v03(card, &url)
    } else {
        serde_json::to_value(card).unwrap_or(Value::Null)
    };
    let tag = etag(&body);
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == tag));
    let mut resp = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Json(body).into_response()
    };
    let h = resp.headers_mut();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    if let Ok(v) = HeaderValue::from_str(&tag) {
        h.insert(header::ETAG, v);
    }
    resp
}

async fn root_card(State(s): State<Arc<A2aState>>, headers: HeaderMap, q: Q) -> Response {
    match s.cards.root() {
        Some((_, c)) => card_response(c, &headers, &q, false),
        None => not_found("/"),
    }
}

async fn root_card_v03(State(s): State<Arc<A2aState>>, headers: HeaderMap, q: Q) -> Response {
    match s.cards.root() {
        Some((_, c)) => card_response(c, &headers, &q, true),
        None => not_found("/"),
    }
}

async fn planner_card(State(s): State<Arc<A2aState>>, headers: HeaderMap, q: Q) -> Response {
    match &s.cards.planner {
        Some(c) => card_response(c, &headers, &q, false),
        None => not_found("/a2a"),
    }
}

async fn planner_card_v03(State(s): State<Arc<A2aState>>, headers: HeaderMap, q: Q) -> Response {
    match &s.cards.planner {
        Some(c) => card_response(c, &headers, &q, true),
        None => not_found("/a2a"),
    }
}

async fn agent_card(
    State(s): State<Arc<A2aState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    q: Q,
) -> Response {
    match s.cards.agents.get(&name) {
        Some(c) => card_response(c, &headers, &q, false),
        None => not_found(&format!("/a2a/agents/{name}")),
    }
}

async fn agent_card_v03(
    State(s): State<Arc<A2aState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    q: Q,
) -> Response {
    match s.cards.agents.get(&name) {
        Some(c) => card_response(c, &headers, &q, true),
        None => not_found(&format!("/a2a/agents/{name}")),
    }
}

async fn planner_rpc(State(s): State<Arc<A2aState>>, headers: HeaderMap, body: Bytes) -> Response {
    if s.cards.planner.is_none() {
        return not_found("/a2a");
    }
    rpc(&s, A2aTarget::Planner, &headers, &body).await
}

async fn agent_rpc(
    State(s): State<Arc<A2aState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !s.cards.agents.contains_key(&name) {
        return not_found(&format!("/a2a/agents/{name}"));
    }
    rpc(&s, A2aTarget::Agent(name), &headers, &body).await
}

/// Constant-time byte comparison (length leaks only).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Authorization` matches `Bearer <token>` (the scheme in any case).
fn authorized(expected: &str, headers: &HeaderMap) -> bool {
    let Some(got) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
    else {
        return false;
    };
    let normalized = match got.split_once(' ') {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => {
            format!("Bearer {}", rest.trim())
        }
        _ => got.to_string(),
    };
    ct_eq(normalized.as_bytes(), expected.as_bytes())
}

async fn rpc(s: &A2aState, target: A2aTarget, headers: &HeaderMap, body: &Bytes) -> Response {
    if let Some(expected) = &s.expected_auth {
        if !authorized(expected, headers) {
            warn!(target: "tengu::a2a", endpoint = %target.label(), "a2a request refused: missing or wrong bearer token");
            let mut resp = (
                StatusCode::UNAUTHORIZED,
                Json(json!({"jsonrpc": "2.0", "id": null, "error": {
                    "code": -32600,
                    "message": "Unauthorized: send `Authorization: Bearer <token>` (the agent card's `bearer` scheme)",
                }})),
            )
                .into_response();
            resp.headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            return resp;
        }
    }
    let version = headers
        .get("a2a-version")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match s.service.handle(&target, version.as_deref(), body).await {
        Reply::Json(v) => Json(v).into_response(),
        Reply::Stream {
            id,
            dialect,
            events,
        } => {
            let stream = tokio_stream::wrappers::ReceiverStream::new(events).map(move |e| {
                Ok::<_, std::convert::Infallible>(
                    Event::default().data(stream_event(&id, dialect, &e).to_string()),
                )
            });
            Sse::new(stream)
                .keep_alive(KeepAlive::default())
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::a2a::Limits;
    use crate::ports::a2a::{A2aRunner, A2aTurn};
    use async_trait::async_trait;
    use std::time::Duration;

    struct Echo;

    #[async_trait]
    impl A2aRunner for Echo {
        async fn run(&self, target: &A2aTarget, turn: A2aTurn) -> anyhow::Result<String> {
            Ok(format!("{} heard: {}", target.label(), turn.text))
        }
    }

    const CONFIG: &str = r#"
[orchestrator]
agent = "planner"

[agents.planner]
engine = "openrouter"
model = "x/y"
default = true

[agents.writer]
engine = "openrouter"
model = "x/y"
description = "Writes things."

[a2a.server]
token_env = "TENGU_A2A_TEST_TOKEN_UNUSED"
public_url = "http://127.0.0.1:9"
orchestrator = true
agents = ["writer"]
"#;

    /// The router on a loopback port, an `Echo` runner behind it.
    async fn serve(auth: Option<&str>) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CONFIG).unwrap();
        let config = Config::load(&path).unwrap();
        let cards = crate::bootstrap::a2a::cards(&config).unwrap();
        let service = A2aService::new(
            Arc::new(Echo),
            Arc::new(crate::adapters::outbound::clock::SystemClock),
            Limits {
                max_tasks: 10,
                max_running: 2,
                context_turns: 4,
                run_timeout: Duration::from_secs(5),
            },
        );
        let state = Arc::new(A2aState {
            cards,
            service,
            expected_auth: auth.map(|t| format!("Bearer {t}")),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        url
    }

    fn send(text: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": {"message": {
            "messageId": "m-1", "role": "ROLE_USER", "parts": [{"text": text}]}}})
    }

    #[tokio::test]
    async fn cards_are_public_and_negotiated() {
        let url = serve(Some("s3cret-token")).await;
        let http = reqwest::Client::new();
        let root = http
            .get(format!("{url}/.well-known/agent-card.json"))
            .send()
            .await
            .unwrap();
        assert_eq!(root.status(), 200);
        let tag = root.headers()["etag"].to_str().unwrap().to_string();
        let v: Value = root.json().await.unwrap();
        assert_eq!(v["supportedInterfaces"][0]["protocolVersion"], "1.0");
        assert_eq!(v["supportedInterfaces"][0]["url"], "http://127.0.0.1:9/a2a");
        let again = http
            .get(format!("{url}/.well-known/agent-card.json"))
            .header("If-None-Match", &tag)
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), 304);
        let old: Value = http
            .get(format!("{url}/a2a/agents/writer/.well-known/agent.json"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(old["protocolVersion"], "0.3.0");
        assert_eq!(old["url"], "http://127.0.0.1:9/a2a/agents/writer");
        assert_eq!(old["securitySchemes"]["bearer"]["type"], "http");
        let by_header: Value = http
            .get(format!("{url}/.well-known/agent-card.json"))
            .header("A2A-Version", "0.3")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(by_header.get("supportedInterfaces").is_none());
        let missing = http
            .get(format!(
                "{url}/a2a/agents/ghost/.well-known/agent-card.json"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
    }

    #[tokio::test]
    async fn rpc_needs_the_bearer_token() {
        let url = serve(Some("s3cret-token")).await;
        let http = reqwest::Client::new();
        let no = http
            .post(format!("{url}/a2a"))
            .json(&send("hi"))
            .send()
            .await
            .unwrap();
        assert_eq!(no.status(), 401);
        assert_eq!(no.headers()["www-authenticate"], "Bearer");
        let wrong = http
            .post(format!("{url}/a2a"))
            .bearer_auth("nope")
            .json(&send("hi"))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), 401);
        let ok: Value = http
            .post(format!("{url}/a2a/agents/writer"))
            .header("Authorization", "bearer s3cret-token")
            .header("A2A-Version", "1.0")
            .json(&send("hi"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            ok["result"]["task"]["artifacts"][0]["parts"][0]["text"],
            "agent:writer heard: hi"
        );
        let ghost = http
            .post(format!("{url}/a2a/agents/ghost"))
            .bearer_auth("s3cret-token")
            .json(&send("hi"))
            .send()
            .await
            .unwrap();
        assert_eq!(ghost.status(), 404);
    }

    #[tokio::test]
    async fn streams_as_server_sent_events() {
        let url = serve(None).await;
        let mut body = send("stream me");
        body["method"] = json!("SendStreamingMessage");
        let resp = reqwest::Client::new()
            .post(format!("{url}/a2a"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        let text = resp.text().await.unwrap();
        let events: Vec<Value> = text
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .map(|d| serde_json::from_str(d).unwrap())
            .collect();
        assert!(events[0]["result"].get("task").is_some(), "{text}");
        assert_eq!(
            events.last().unwrap()["result"]["statusUpdate"]["status"]["state"],
            "TASK_STATE_COMPLETED"
        );
        assert!(text.contains("planner heard: stream me"), "{text}");
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("BEARER  tok"),
        );
        assert!(authorized("Bearer tok", &h));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic tok"));
        assert!(!authorized("Bearer tok", &h));
    }
}
