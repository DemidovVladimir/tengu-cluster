//! Studio server tests (ST-20): the real router on a `127.0.0.1:0`
//! listener, driven with reqwest; a temp `TENGU_HOME` per test (trace dir,
//! kept maps, heartbeat), the `control-loop-lab` sandbox config.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderName, HOST, ORIGIN};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio::sync::{watch, Semaphore};

use super::guard::{loopback, Token, TOKEN_HEADER};
use super::{router, run_studio, AppState, ServeOpts};
use crate::adapters::outbound::runtime_store::write_heartbeat;
use crate::adapters::outbound::trace_store::{trace_root, JsonlTraceSink};
use crate::bootstrap::runtime::read_live;
use crate::bootstrap::studio::StudioContext;
use crate::config::execution_map::ExecutionMap;
use crate::config::Config;
use crate::domain::observation::now_ms;
use crate::domain::runtime::{Heartbeat, RunState};
use crate::domain::secrets::SecretRegistry;
use crate::domain::trace::{Component, EventDraft, RunKind, Status};
use crate::ports::trace::TraceSink;

const SANDBOX: &str = "control-loop-lab";
const WAIT: Duration = Duration::from_secs(10);

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn lab() -> Config {
    let path = repo().join("sandboxes").join(SANDBOX).join("config.toml");
    let mut cfg = Config::load(&path).unwrap_or_else(|e| panic!("{SANDBOX}: {e:#}"));
    cfg.sandbox_name = Some(SANDBOX.to_string());
    cfg
}

/// One Studio server on a free loopback port, its home a temp dir.
struct Studio {
    base: String,
    port: u16,
    token: String,
    home: tempfile::TempDir,
    config_hash: String,
    stop: watch::Sender<bool>,
    http: reqwest::Client,
}

impl Studio {
    async fn start() -> Self {
        Self::with(|_| {}).await
    }

    /// `tune` adjusts the state before serving (limits, stream cap).
    async fn with(tune: impl FnOnce(&mut AppState)) -> Self {
        let home = tempfile::tempdir().unwrap();
        let state_dir = home.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let config = lab();
        let config_hash = config.source_sha256.clone().expect("loaded from a file");
        let ctx = StudioContext::at(
            config,
            Arc::new(SecretRegistry::new()),
            home.path(),
            &state_dir,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stop_rx) = watch::channel(false);
        let token = Token::generate().unwrap();
        let mut st = AppState::new(ctx, token.clone(), addr, stop_rx.clone());
        tune(&mut st);
        let app = router(Arc::new(st));
        let mut wait = stop_rx;
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = wait.wait_for(|s| *s).await;
                })
                .await
        });
        Self {
            base: format!("http://{addr}"),
            port: addr.port(),
            token: token.as_str().to_string(),
            home,
            config_hash,
            stop,
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
        }
    }

    fn state_dir(&self) -> PathBuf {
        self.home.path().join("state")
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// GET with the token header.
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(self.url(path))
            .header(TOKEN_HEADER, &self.token)
    }

    async fn json(&self, path: &str) -> (StatusCode, Value) {
        let r = self.get(path).send().await.unwrap();
        let code = r.status();
        (code, r.json().await.unwrap_or(Value::Null))
    }

    /// A recording under this home, as `tengu run` (a runtime id) or
    /// `tengu decide` (none) opens it.
    fn sink(&self, runtime_id: Option<&str>) -> JsonlTraceSink {
        JsonlTraceSink::open(
            &trace_root(self.home.path()),
            SANDBOX,
            Some(&self.config_hash),
            runtime_id,
            if runtime_id.is_some() {
                RunKind::Run
            } else {
                RunKind::Decide
            },
            Arc::new(SecretRegistry::new()),
        )
        .unwrap()
    }

    fn beat(&self, holder: &str) {
        let now = now_ms();
        write_heartbeat(
            &self.state_dir(),
            &Heartbeat {
                sandbox: SANDBOX.into(),
                pid: 4242,
                holder: holder.into(),
                state: RunState::Running,
                stop_reason: None,
                started_at_ms: now - 5_000,
                ts_ms: now,
                heartbeat_secs: 2,
                loops: Default::default(),
                feeds: Default::default(),
            },
        )
        .unwrap();
    }

    /// An SSE stream (`path` + the token header, `headers` added).
    async fn sse(&self, path: &str, headers: &[(&str, String)]) -> Sse {
        let mut req = self.get(path);
        for (k, v) in headers {
            req = req.header(HeaderName::from_bytes(k.as_bytes()).unwrap(), v);
        }
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        assert_eq!(
            resp.headers()["content-type"].to_str().unwrap(),
            "text/event-stream"
        );
        Sse {
            resp,
            buf: Vec::new(),
        }
    }
}

fn emit(s: &JsonlTraceSink, n: u64, pad: usize) {
    for i in 0..n {
        s.emit(
            EventDraft::new(Component::Loop, "loop.completed", Status::Ok)
                .session(format!("tick:{i}"))
                .node("loop:demo")
                .payload(json!({"i": i, "pad": "x".repeat(pad)})),
        )
        .unwrap();
    }
}

/// One server-sent event.
#[derive(Debug)]
struct Frame {
    id: Option<String>,
    event: String,
    data: Value,
}

impl Frame {
    fn seq(&self) -> u64 {
        assert_eq!(self.event, "trace", "{self:?}");
        self.data["seq"].as_u64().unwrap()
    }
}

struct Sse {
    resp: reqwest::Response,
    buf: Vec<u8>,
}

impl Sse {
    /// The next event (keep-alive comments skipped); `None` = stream ended.
    async fn next(&mut self) -> Option<Frame> {
        loop {
            if let Some(i) = self.buf.windows(2).position(|w| w == b"\n\n") {
                let block: Vec<u8> = self.buf.drain(..i + 2).collect();
                let text = String::from_utf8(block).unwrap();
                let (mut id, mut event, mut data) = (None, "message".to_string(), String::new());
                for line in text.lines() {
                    let field = |p: &str| line.strip_prefix(p).map(|v| v.trim_start().to_string());
                    if let Some(v) = field("id:") {
                        id = Some(v);
                    } else if let Some(v) = field("event:") {
                        event = v;
                    } else if let Some(v) = field("data:") {
                        data.push_str(&v);
                    }
                }
                if data.is_empty() {
                    continue; // a keep-alive comment
                }
                let data = serde_json::from_str(&data).unwrap();
                return Some(Frame { id, event, data });
            }
            let chunk = tokio::time::timeout(WAIT, self.resp.chunk())
                .await
                .expect("SSE stream timed out")
                .ok()??;
            self.buf.extend_from_slice(&chunk);
        }
    }

    async fn frame(&mut self) -> Frame {
        self.next().await.expect("the stream ended")
    }
}

/// `--bind` other than loopback is refused before anything binds.
#[tokio::test]
async fn refuses_non_loopback_bind() {
    for bad in [
        "0.0.0.0",
        "::",
        "192.168.1.20",
        "10.0.0.1",
        "example.com",
        "",
    ] {
        assert!(loopback(bad).is_err(), "{bad}");
    }
    for ok in ["127.0.0.1", "127.0.0.2", "::1", "[::1]", "localhost"] {
        assert!(loopback(ok).unwrap().is_loopback(), "{ok}");
    }
    let opts = ServeOpts {
        bind: "0.0.0.0".into(),
        port: 0,
    };
    let err = run_studio(lab(), Arc::new(SecretRegistry::new()), opts)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("loopback only"), "{err:#}");
}

/// DNS rebinding: a `Host` that is not this loopback server is 421, on
/// the page and the API alike; `127.0.0.1:<port>` / `localhost:<port>` pass.
#[tokio::test]
async fn rejects_foreign_host_header() {
    let s = Studio::start().await;
    let foreign = [
        "evil.example".to_string(),
        format!("evil.example:{}", s.port),
        "127.0.0.1:1".to_string(),
        format!("10.0.0.1:{}", s.port),
    ];
    for host in &foreign {
        for path in ["/", "/api/v1/meta"] {
            let r = s.get(path).header(HOST, host).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::MISDIRECTED_REQUEST, "{host} {path}");
        }
    }
    for host in [
        format!("127.0.0.1:{}", s.port),
        format!("LOCALHOST:{}", s.port),
    ] {
        let r = s
            .get("/api/v1/meta")
            .header(HOST, &host)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{host}");
    }
}

/// A foreign `Origin` or a cross-site fetch is 403; this server's own
/// origin and a same-origin fetch pass; no CORS header is ever sent.
#[tokio::test]
async fn rejects_cross_origin() {
    let s = Studio::start().await;
    let own = format!("http://127.0.0.1:{}", s.port);
    for origin in [
        "http://evil.example",
        "null",
        "https://127.0.0.1",
        &format!("http://127.0.0.1:{}", s.port + 1),
    ] {
        for path in ["/", "/api/v1/meta", "/api/v1/live/stream"] {
            let r = s.get(path).header(ORIGIN, origin).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "{origin} {path}");
        }
    }
    for site in ["cross-site", "same-site"] {
        let r = s
            .get("/api/v1/runs")
            .header("sec-fetch-site", site)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{site}");
    }
    let r = s
        .get("/api/v1/runs")
        .header(ORIGIN, &own)
        .header("sec-fetch-site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.headers().get("access-control-allow-origin").is_none());
}

/// Every `/api/` route needs the token (header, or `?token=` for an
/// `EventSource`); the page and its assets do not (the token reaches the
/// page in the URL fragment). Security headers on every answer.
#[tokio::test]
async fn token_guards_every_api_route() {
    let s = Studio::start().await;
    let run = s.sink(None);
    let id = run.run_id().unwrap().to_string();
    let routes = [
        "/api/v1/meta".to_string(),
        "/api/v1/graph".into(),
        "/api/v1/health".into(),
        "/api/v1/runs".into(),
        format!("/api/v1/runs/{id}/events"),
        format!("/api/v1/runs/{id}/board"),
        "/api/v1/nodes/loop:demo".into(),
        format!("/api/v1/runs/{id}/stream"),
        "/api/v1/live/stream".into(),
    ];
    let wrong = "0".repeat(64);
    for path in &routes {
        let bare = s.http.get(s.url(path)).send().await.unwrap();
        assert_eq!(bare.status(), StatusCode::UNAUTHORIZED, "{path}");
        let bad = s
            .http
            .get(s.url(path))
            .header(TOKEN_HEADER, &wrong)
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::UNAUTHORIZED, "{path}");
        for h in [
            "content-security-policy",
            "x-frame-options",
            "cache-control",
            "x-content-type-options",
            "referrer-policy",
        ] {
            assert!(bad.headers().get(h).is_some(), "{path}: {h}");
        }
    }
    let sep = |p: &str| if p.contains('?') { '&' } else { '?' };
    for path in routes.iter().filter(|p| !p.ends_with("stream")) {
        let url = format!("{}{}token={}", s.url(path), sep(path), s.token);
        let r = s.http.get(url).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{path} ?token=");
    }
    // The page and assets: embedded, no token, strict headers.
    let page = s.http.get(s.url("/")).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let csp = page.headers()["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("default-src 'self'") && csp.contains("frame-ancestors 'none'"));
    assert_eq!(page.headers()["x-frame-options"], "DENY");
    assert_eq!(page.headers()["cache-control"], "no-store");
    assert!(page.text().await.unwrap().contains("/assets/studio.js"));
    for (asset, kind) in [
        ("studio.js", "text/javascript; charset=utf-8"),
        ("studio.css", "text/css; charset=utf-8"),
    ] {
        let r = s
            .http
            .get(s.url(&format!("/assets/{asset}")))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{asset}");
        assert_eq!(r.headers()["content-type"], kind);
    }
    for missing in ["/assets/nope.js", "/assets/..%2Fmod.rs", "/assets/"] {
        let r = s.http.get(s.url(missing)).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{missing}");
    }
}

/// Read-only: the meta says so, and every method but GET / HEAD is 405 on
/// every route (no control route exists: 404).
#[tokio::test]
async fn get_routes_only_without_control() {
    let s = Studio::start().await;
    let (code, meta) = s.json("/api/v1/meta").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(meta["read_only"], json!(true));
    assert_eq!(meta["control_enabled"], json!(false));
    assert_eq!(meta["sandbox"], json!(SANDBOX));
    assert_eq!(meta["config_hash"], json!(s.config_hash));
    assert_eq!(meta["schema"], json!({"workflow": 1, "trace": 1}));
    // The colour legend comes from Rust (`Status::tone`), not the page.
    let tone_of = |status: &str| {
        meta["tones"]["statuses"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["status"] == json!(status))
            .map(|r| r["tone"].clone())
    };
    assert_eq!(tone_of("ok"), Some(json!("green")));
    assert_eq!(tone_of("refused"), Some(json!("red")));
    assert_eq!(tone_of("escalated"), Some(json!("amber")));
    assert_eq!(tone_of("skipped"), Some(json!("plain")));
    assert_eq!(meta["tones"]["tones"].as_array().unwrap().len(), 5);
    let trace_dir = meta["evidence"]["trace_dir"].as_str().unwrap();
    assert!(
        trace_dir.ends_with("logs/trace/control-loop-lab"),
        "{trace_dir}"
    );
    for path in [
        "/",
        "/api/v1/meta",
        "/api/v1/graph",
        "/api/v1/runs",
        "/api/v1/health",
    ] {
        for method in ["POST", "PUT", "DELETE", "PATCH"] {
            let r = s
                .http
                .request(method.parse().unwrap(), s.url(path))
                .header(TOKEN_HEADER, &s.token)
                .body("{}")
                .send()
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path}"
            );
        }
    }
    for path in ["/api/v1/control/play", "/api/v1/control/stop"] {
        let r = s
            .http
            .post(s.url(path))
            .header(TOKEN_HEADER, &s.token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path}");
    }
    let head = s
        .http
        .head(s.url("/api/v1/meta"))
        .header(TOKEN_HEADER, &s.token);
    assert_eq!(head.send().await.unwrap().status(), StatusCode::OK);
}

/// `/api/v1/graph` is the golden graph of the lab (the same read model as
/// `tengu studio graph`); `?map=` serves a map `tengu decide --map` kept,
/// re-hashed and re-applied; a bad, unknown, tampered or refused map says
/// why.
#[tokio::test]
async fn graph_endpoint_matches_golden() {
    let s = Studio::start().await;
    let (code, got) = s.json("/api/v1/graph").await;
    assert_eq!(code, StatusCode::OK);
    let golden = repo().join("tests/fixtures/studio/graph-control-loop-lab.json");
    let want: Value = serde_json::from_str(&std::fs::read_to_string(golden).unwrap()).unwrap();
    assert_eq!(got, want);

    let maps = s.home.path().join("logs").join("maps");
    std::fs::create_dir_all(&maps).unwrap();
    let keep = |text: &str| {
        let map = ExecutionMap::parse(text).unwrap();
        let sha = map.sha256();
        std::fs::write(maps.join(format!("{sha}.json")), map.canonical()).unwrap();
        sha
    };
    let text = std::fs::read_to_string(
        repo().join("sandboxes/control-loop-lab/scenarios/uncertain.map.json"),
    )
    .unwrap();
    let sha = keep(&text);
    let (code, g) = s.json(&format!("/api/v1/graph?map={sha}")).await;
    assert_eq!(code, StatusCode::OK, "{g}");
    assert_eq!(g["map"], json!({"sha256": sha, "loop": "demo"}));
    assert_ne!(g, want);

    let (code, _) = s.json("/api/v1/graph?map=xyz").await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = s
        .json(&format!("/api/v1/graph?map={}", "0".repeat(64)))
        .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    // A file whose content does not hash to its name.
    let fake = "a".repeat(64);
    std::fs::write(maps.join(format!("{fake}.json")), text.trim()).unwrap();
    let (code, e) = s.json(&format!("/api/v1/graph?map={fake}")).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    // A map this config refuses: every reason listed.
    let refused = keep(r#"{"loop":"nope","event":{}}"#);
    let (code, e) = s.json(&format!("/api/v1/graph?map={refused}")).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    assert!(!e["reasons"].as_array().unwrap().is_empty(), "{e}");
}

/// `/events` pages a run in `seq` order with `more` / `next_after`; the
/// runs list names it; bad input is 400, an unknown run 404.
#[tokio::test]
async fn events_page_is_ordered() {
    let s = Studio::start().await;
    let run = s.sink(Some("host:1:0f0e0d0c-0b0a-4908-8706-050403020100"));
    emit(&run, 9, 0);
    let id = run.run_id().unwrap().to_string();
    let mut seen = Vec::new();
    let mut after = 0;
    for (want, more) in [
        (vec![1, 2, 3, 4], true),
        (vec![5, 6, 7, 8], true),
        (vec![9, 10], false),
    ] {
        let (code, page) = s
            .json(&format!("/api/v1/runs/{id}/events?after={after}&limit=4"))
            .await;
        assert_eq!(code, StatusCode::OK);
        let seqs: Vec<u64> = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                assert_eq!(e["event_id"], json!(format!("{id}:{}", e["seq"])));
                e["seq"].as_u64().unwrap()
            })
            .collect();
        assert_eq!(seqs, want);
        assert_eq!(page["more"], json!(more));
        assert_eq!(page["next_after"], json!(want.last().unwrap()));
        after = *want.last().unwrap();
        seen.extend(seqs);
    }
    assert_eq!(seen, (1..=10).collect::<Vec<u64>>());
    let (_, tail) = s.json(&format!("/api/v1/runs/{id}/events?after=10")).await;
    assert_eq!(
        (tail["events"].clone(), tail["more"].clone()),
        (json!([]), json!(false))
    );
    assert_eq!(tail["next_after"], json!(10));

    let (_, runs) = s.json("/api/v1/runs").await;
    let row = &runs["runs"][0];
    assert_eq!(row["run_id"], json!(id));
    assert_eq!(
        (row["events"].clone(), row["last_seq"].clone()),
        (json!(10), json!(10))
    );
    assert_eq!(row["config_current"], json!(true));
    assert_eq!(runs["live_run_id"], Value::Null, "no heartbeat");

    for bad in [
        format!("/api/v1/runs/{id}/events?limit=0"),
        format!("/api/v1/runs/{id}/events?limit=1001"),
        "/api/v1/runs/not-a-run/events".into(),
        format!("/api/v1/runs/{}/events", id.to_uppercase()),
    ] {
        assert_eq!(s.json(&bad).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
    let unknown = "/api/v1/runs/5b0c7d0e-8a4e-4f0a-9d8e-2f1c3b4a5d6e/events";
    assert_eq!(s.json(unknown).await.0, StatusCode::NOT_FOUND);
}

/// A run stream resumes after `Last-Event-ID` (a browser reconnect; wins
/// over `?after=`), then goes on live; ids are `event_id`s.
#[tokio::test]
async fn sse_resumes_from_last_event_id() {
    let s = Studio::start().await;
    let run = s.sink(None);
    emit(&run, 5, 0);
    let id = run.run_id().unwrap().to_string();
    let path = format!("/api/v1/runs/{id}/stream");

    let mut a = s
        .sse(
            &format!("{path}?after=1"),
            &[("last-event-id", format!("{id}:3"))],
        )
        .await;
    for want in 4..=6 {
        let f = a.frame().await;
        assert_eq!((f.seq(), f.id), (want, Some(format!("{id}:{want}"))));
    }
    emit(&run, 1, 0);
    assert_eq!(a.frame().await.seq(), 7, "live after the backlog");

    let mut b = s.sse(&format!("{path}?after=5"), &[]).await;
    assert_eq!(b.frame().await.seq(), 6);
    let mut c = s.sse(&path, &[]).await;
    let first = c.frame().await;
    assert_eq!(
        (first.seq(), first.data["kind"].clone()),
        (1, json!("run.opened"))
    );

    let other = "5b0c7d0e-8a4e-4f0a-9d8e-2f1c3b4a5d6e:3".to_string();
    let r = s
        .get(&path)
        .header("last-event-id", other)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

/// A client that stops reading while live events pour in gets `lagged`
/// (id = the last event it was handed) and the stream ends; reconnecting
/// with that id yields the rest — every `seq` exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_lagged_client_resumes_without_gap() {
    let s = Studio::with(|st| st.limits.client_buffer = 4).await;
    let run = s.sink(None);
    let id = run.run_id().unwrap().to_string();
    let path = format!("/api/v1/runs/{id}/stream");
    let mut first = s.sse(&path, &[]).await;
    assert_eq!(first.frame().await.seq(), 1, "backlog: run.opened");
    // Far more than the socket buffers hold while the client does not
    // read: the server's per-client buffer fills.
    const N: u64 = 5000;
    emit(&run, N, 3000);
    let last = N + 1;
    let mut seqs = vec![1];
    let lag = loop {
        match first.next().await {
            Some(f) if f.event == "trace" => seqs.push(f.seq()),
            Some(f) if f.event == "lagged" => break f,
            other => panic!("expected lagged before the end: {other:?}"),
        }
    };
    let handed = *seqs.last().unwrap();
    assert!(handed < last, "the client lagged before the end");
    assert_eq!(lag.data["last_seq"], json!(handed));
    assert_eq!(lag.id, Some(format!("{id}:{handed}")));
    assert!(first.next().await.is_none(), "the stream ends after lagged");

    let mut again = s
        .sse(&path, &[("last-event-id", lag.id.clone().unwrap())])
        .await;
    while *seqs.last().unwrap() < last {
        let f = again.frame().await;
        seqs.push(f.seq());
    }
    assert_eq!(seqs, (1..=last).collect::<Vec<u64>>(), "no gap, no repeat");
}

/// The live stream follows the run whose `runtime_id` is the heartbeat's
/// lease holder (never a decide run, never an older runtime); a restart
/// (new holder) switches to the new run; `/runs` and `/health` agree.
#[tokio::test]
async fn attaches_to_running_lab_by_heartbeat_holder() {
    let s = Studio::with(|st| st.limits.holder_poll = Duration::from_millis(50)).await;
    let (h0, h1, h2) = (
        "host:10:0f0e0d0c-0b0a-4908-8706-050403020100",
        "host:11:1f0e0d0c-0b0a-4908-8706-050403020100",
        "host:12:2f0e0d0c-0b0a-4908-8706-050403020100",
    );
    emit(&s.sink(None), 2, 0);
    emit(&s.sink(Some(h0)), 2, 0);
    let live = s.sink(Some(h1));
    emit(&live, 2, 0);
    let live_id = live.run_id().unwrap().to_string();

    let mut stream = s.sse("/api/v1/live/stream", &[]).await;
    let waiting = stream.frame().await;
    assert_eq!(waiting.event, "run");
    assert_eq!(waiting.data["reason"], json!("waiting"));
    assert_eq!(waiting.data["run_id"], Value::Null);

    s.beat(h1);
    let attached = stream.frame().await;
    assert_eq!(attached.event, "run");
    assert_eq!(
        (
            attached.data["reason"].clone(),
            attached.data["run_id"].clone()
        ),
        (json!("attached"), json!(live_id))
    );
    assert_eq!(attached.data["runtime_id"], json!(h1));
    for want in 1..=3 {
        let f = stream.frame().await;
        assert_eq!(f.seq(), want);
        assert_eq!(f.data["run_id"], json!(live_id));
    }

    let (_, runs) = s.json("/api/v1/runs").await;
    assert_eq!(runs["holder"], json!(h1));
    assert_eq!(runs["live_run_id"], json!(live_id));
    assert_eq!(runs["runs"].as_array().unwrap().len(), 3);
    let (_, health) = s.json("/api/v1/health").await;
    assert_eq!(health["heartbeat"]["read"], json!("found"));
    assert_eq!(health["heartbeat"]["value"]["holder"], json!(h1));
    let checks = health["checks"].as_array().unwrap();
    let heartbeat = checks.iter().find(|c| c["subject"] == json!("heartbeat"));
    assert_eq!(heartbeat.unwrap()["ok"], json!(true), "{health}");
    // The doctor's own verdict over the same files: same checks.
    let doctor = read_live(&lab(), &s.state_dir()).await;
    let pairs = |v: Vec<(String, bool)>| v;
    assert_eq!(
        pairs(
            checks
                .iter()
                .map(|c| (
                    c["subject"].as_str().unwrap().to_string(),
                    c["ok"].as_bool().unwrap()
                ))
                .collect()
        ),
        doctor
            .report
            .checks
            .iter()
            .map(|c| (c.subject.clone(), c.ok))
            .collect::<Vec<_>>()
    );
    assert_eq!(health["live"], json!(doctor.report.ok()));

    // Restart: a new holder and its recording.
    let next = s.sink(Some(h2));
    s.beat(h2);
    let restarted = stream.frame().await;
    assert_eq!(restarted.event, "run");
    assert_eq!(restarted.data["reason"], json!("restarted"));
    assert_eq!(restarted.data["run_id"], json!(next.run_id().unwrap()));
    let f = stream.frame().await;
    assert_eq!((f.seq(), f.data["runtime_id"].clone()), (1, json!(h2)));
}

/// No runtime under this home: the doctor's verdict, not live.
#[tokio::test]
async fn health_without_a_runtime_is_not_live() {
    let s = Studio::start().await;
    let (code, h) = s.json("/api/v1/health").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(h["live"], json!(false));
    assert_eq!(h["heartbeat"]["read"], json!("missing"));
    let hb = h["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["subject"] == json!("heartbeat"))
        .unwrap();
    assert_eq!(hb["ok"], json!(false));
    assert!(h["heartbeat"]["file"]
        .as_str()
        .unwrap()
        .ends_with("state/run-control-loop-lab.json"));
}

/// Streams are capped (503 past the cap) and all end when the server stops.
#[tokio::test]
async fn streams_are_capped_and_end_at_shutdown() {
    let s = Studio::with(|st| st.streams = Arc::new(Semaphore::new(1))).await;
    let mut one = s.sse("/api/v1/live/stream", &[]).await;
    assert_eq!(one.frame().await.data["reason"], json!("waiting"));
    let two = s.get("/api/v1/live/stream").send().await.unwrap();
    assert_eq!(two.status(), StatusCode::SERVICE_UNAVAILABLE);
    s.stop.send(true).unwrap();
    assert!(one.next().await.is_none(), "the stream ends at shutdown");
}

// --- ST-21 / ST-22: board, inspector, replay ---------------------------------

const MODEL: &str = "typesafe/jev-1.13-20260917";

/// A recording with its own `config_hash` (a run of an older config).
fn sink_with_hash(s: &Studio, hash: &str, runtime_id: Option<&str>) -> JsonlTraceSink {
    JsonlTraceSink::open(
        &trace_root(s.home.path()),
        SANDBOX,
        Some(hash),
        runtime_id,
        if runtime_id.is_some() {
            RunKind::Run
        } else {
            RunKind::Decide
        },
        Arc::new(SecretRegistry::new()),
    )
    .unwrap()
}

/// One tick of the lab's `act` path, as `tengu run` records it: the feed
/// fires, the loop runs, Jev picks write_marker, the tool writes.
fn tick(s: &JsonlTraceSink, n: u64) {
    let sess = format!("tick:{n}");
    let d = |kind: &str, status: Status, node: &str| {
        EventDraft::new(Component::Loop, kind, status)
            .session(sess.clone())
            .node(node)
    };
    let fired = s
        .emit(d("feed.fired", Status::Running, "feed:tick"))
        .unwrap();
    let queued = s
        .emit(
            d("loop.queued", Status::Pending, "loop:demo")
                .parent(fired)
                .payload(json!({"stats": {"queued": 1, "completed": n - 1}})),
        )
        .unwrap();
    let started = s
        .emit(d("loop.started", Status::Running, "loop:demo").parent(queued))
        .unwrap();
    let jev = s
        .emit(
            d("jev.completed", Status::Ok, "jev:demo")
                .parent(started.clone())
                .payload(json!({
                    "model": MODEL,
                    "decision_id": format!("gen-dec-{n}"),
                    "legal_actions": ["hold", "read_probe", "write_marker"],
                    "answers": {"next_action": {"choice": "write_marker", "confidence": 0.93}},
                })),
        )
        .unwrap();
    let call = format!("demo:{sess}:1");
    let sel = s
        .emit(
            d(
                "action.selected",
                Status::Running,
                "action:demo/write_marker",
            )
            .parent(jev)
            .call(call.clone()),
        )
        .unwrap();
    let ts = s
        .emit(
            d("tool.started", Status::Running, "tool:lab/write_file")
                .parent(sel.clone())
                .call(call.clone()),
        )
        .unwrap();
    s.emit(
        d("tool.completed", Status::Ok, "tool:lab/write_file")
            .parent(ts)
            .call(call.clone())
            .duration(1),
    );
    s.emit(
        d("action.completed", Status::Ok, "action:demo/write_marker")
            .parent(sel)
            .call(call),
    );
    s.emit(
        d("loop.completed", Status::Ok, "loop:demo")
            .parent(started)
            .payload(json!({"stats": {"queued": 0, "completed": n}})),
    );
}

/// `runtime.stopping` → `runtime.stopped`: the run is closed.
fn stop(s: &JsonlTraceSink) {
    let node = format!("runtime:{SANDBOX}");
    let stopping = s
        .emit(
            EventDraft::new(Component::Runtime, "runtime.stopping", Status::Pending)
                .node(node.clone()),
        )
        .unwrap();
    s.emit(
        EventDraft::new(Component::Runtime, "runtime.stopped", Status::Ok)
            .node(node)
            .parent(stopping),
    );
}

/// Every event of a run through `/events` pages of `limit`.
async fn replay(s: &Studio, run_id: &str, limit: usize) -> Vec<Value> {
    let mut out = Vec::new();
    let mut after = 0;
    loop {
        let (code, page) = s
            .json(&format!(
                "/api/v1/runs/{run_id}/events?after={after}&limit={limit}"
            ))
            .await;
        assert_eq!(code, StatusCode::OK, "{page}");
        out.extend(page["events"].as_array().unwrap().iter().cloned());
        after = page["next_after"].as_u64().unwrap();
        if page["more"] == json!(false) {
            return out;
        }
    }
}

/// An event as written (its `view` removed).
fn bare(ev: &Value) -> Value {
    let mut ev = ev.clone();
    ev.as_object_mut().unwrap().remove("view");
    ev
}

/// The inspector reads the validated config, never the TOML text: a
/// default the TOML never wrote is there, a comment is not; a map's node
/// shows the narrowed loop; ids may come percent-encoded; unknown = 404.
#[tokio::test]
async fn node_detail_is_from_validated_config() {
    let s = Studio::start().await;
    let enc = |id: &str| id.replace(':', "%3A").replace('/', "%2F");
    let (code, d) = s.json(&format!("/api/v1/nodes/{}", enc("loop:demo"))).await;
    assert_eq!(code, StatusCode::OK, "{d}");
    assert_eq!(d["config"]["section"], json!("decision_loops.demo"));
    assert_eq!(
        d["config"]["value"]["timeout_secs"],
        json!(20),
        "a default, not in the TOML"
    );
    assert_eq!(d["config"]["value"]["act_at"], json!(0.8));
    assert_eq!(
        d["config"]["value"]["actions"],
        json!(["hold", "read_probe", "write_marker"])
    );
    assert_eq!(d["source"], json!("validated config (Config::load)"));
    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(repo().join("tests/fixtures/studio/graph-control-loop-lab.json"))
            .unwrap(),
    )
    .unwrap();
    let node = golden["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == json!("loop:demo"))
        .unwrap();
    assert_eq!(&d["node"], node, "the graph's own node");
    let labels: Vec<&str> = d["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["label"].as_str().unwrap())
        .collect();
    assert!(
        labels.contains(&"heartbeat (tengu doctor --live)"),
        "{labels:?}"
    );
    assert!(
        labels.contains(&"decision audit (decisions.jsonl)"),
        "{labels:?}"
    );
    let row = d["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == json!("loop/1:demo"))
        .unwrap();
    assert_eq!(
        row["path"],
        json!("~/tengu-lab/control-loop-lab/.tengu/observations.db")
    );
    assert!(d["edges"]["in"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["from"] == json!("feed:tick")));

    // Every node of the graph answers; none carries the TOML's comments.
    let toml =
        std::fs::read_to_string(repo().join("sandboxes/control-loop-lab/config.toml")).unwrap();
    assert!(toml.contains("Short beats so a demo shows health"));
    for n in golden["nodes"].as_array().unwrap() {
        let id = n["id"].as_str().unwrap();
        let r = s
            .get(&format!("/api/v1/nodes/{}", enc(id)))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{id}");
        let text = r.text().await.unwrap();
        for comment in [
            "Short beats so a demo shows health",
            "QUICKSTART",
            "the safe reference run",
        ] {
            assert!(!text.contains(comment), "{id}: {comment}");
        }
    }
    let (_, wm) = s.json("/api/v1/nodes/action:demo%2Fwrite_marker").await;
    assert_eq!(
        wm["config"]["section"],
        json!("decision_loops.demo.actions.write_marker")
    );
    assert_eq!(
        wm["config"]["value"]["args"]["path"],
        json!("out/marker.txt")
    );
    // `top` is the schema default the TOML never wrote.
    assert_eq!(
        wm["config"]["value"]["slots"]["scenario"],
        json!({"event": "/scenario", "top": 5})
    );
    let (_, tool) = s
        .json(&format!("/api/v1/nodes/{}", enc("tool:lab/write_file")))
        .await;
    assert_eq!(tool["config"]["section"], json!("catalog.write_file"));
    assert!(!tool["config"]["value"]["description"]
        .as_str()
        .unwrap()
        .is_empty());
    let (_, scope) = s
        .json(&format!("/api/v1/nodes/{}", enc("scope:lab/write_file")))
        .await;
    assert_eq!(
        scope["config"]["value"]["fs_roots"],
        json!(["~/tengu-lab/control-loop-lab/out"])
    );
    let (code, none) = s
        .json(&format!("/api/v1/nodes/{}", enc("trigger:decide")))
        .await;
    assert_eq!(
        (code, none["config"].clone()),
        (StatusCode::OK, Value::Null)
    );

    // A kept map: the narrowed loop, its source named.
    let maps = s.home.path().join("logs").join("maps");
    std::fs::create_dir_all(&maps).unwrap();
    let text = std::fs::read_to_string(
        repo().join("sandboxes/control-loop-lab/scenarios/uncertain.map.json"),
    )
    .unwrap();
    let map = ExecutionMap::parse(&text).unwrap();
    std::fs::write(maps.join(format!("{}.json", map.sha256())), map.canonical()).unwrap();
    let (code, m) = s
        .json(&format!(
            "/api/v1/nodes/{}?map={}",
            enc("loop:demo"),
            map.sha256()
        ))
        .await;
    assert_eq!(code, StatusCode::OK, "{m}");
    assert_eq!(m["config"]["value"]["act_at"], json!(1.0));
    assert!(m["source"]
        .as_str()
        .unwrap()
        .contains("ExecutionMap::apply"));
    let (code, t) = s
        .json(&format!(
            "/api/v1/nodes/{}?map={}",
            enc(&format!("trigger:map/{}", map.sha256())),
            map.sha256()
        ))
        .await;
    assert_eq!(code, StatusCode::OK, "{t}");

    for (path, want) in [
        (
            format!("/api/v1/nodes/{}", enc("loop:nope")),
            StatusCode::NOT_FOUND,
        ),
        (
            format!("/api/v1/nodes/{}?map=xyz", enc("loop:demo")),
            StatusCode::BAD_REQUEST,
        ),
        (
            format!("/api/v1/nodes/{}?map={}", enc("loop:demo"), "0".repeat(64)),
            StatusCode::NOT_FOUND,
        ),
    ] {
        assert_eq!(s.json(&path).await.0, want, "{path}");
    }
    let bare = s
        .http
        .get(s.url("/api/v1/nodes/loop:demo"))
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status(), StatusCode::UNAUTHORIZED);
}

/// Live and replay are one sequence: what the live stream handed while
/// the run was written equals the `/events` pages read after it — the same
/// ids, the same order, the same events; the board folds to the same state
/// at the end whichever way it is asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_equals_live_sequence() {
    let s = Studio::with(|st| st.limits.holder_poll = Duration::from_millis(50)).await;
    let h = "host:20:3f0e0d0c-0b0a-4908-8706-050403020100";
    let run = s.sink(Some(h));
    let id = run.run_id().unwrap().to_string();
    s.beat(h);
    let mut live = s.sse("/api/v1/live/stream", &[]).await;
    loop {
        let f = live.frame().await;
        if f.event == "run" && f.data["reason"] == json!("attached") {
            assert_eq!(f.data["run_id"], json!(id));
            break;
        }
    }
    let mut got = Vec::new();
    for n in 1..=3 {
        tick(&run, n);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    stop(&run);
    let last = 1 + 3 * 9 + 2;
    while got.len() < last {
        let f = live.frame().await;
        assert_eq!(f.event, "trace", "{f:?}");
        assert_eq!(f.id.as_deref(), f.data["event_id"].as_str());
        got.push(f.data);
    }
    let pages = replay(&s, &id, 7).await;
    assert_eq!(pages.len(), last);
    let ids = |v: &[Value]| {
        v.iter()
            .map(|e| e["event_id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&got), ids(&pages));
    for (l, r) in got.iter().zip(&pages) {
        assert_eq!(l, &bare(r));
    }
    assert_eq!(
        pages
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        (1..=last as u64).collect::<Vec<_>>()
    );
    // The views: a tool call knows its loop, action, feed and model.
    let tool = pages
        .iter()
        .find(|e| e["kind"] == json!("tool.completed"))
        .unwrap();
    let v = &tool["view"];
    assert_eq!(
        (
            v["loop"].clone(),
            v["action"].clone(),
            v["feed"].clone(),
            v["model"].clone(),
            v["tone"].clone()
        ),
        (
            json!("demo"),
            json!("write_marker"),
            json!("tick"),
            json!(MODEL),
            json!("green")
        )
    );
    assert_eq!(
        v["edges"],
        json!([{"from": "action:demo/write_marker", "to": "tool:lab/write_file", "kind": "calls"}])
    );

    let (_, all) = s.json(&format!("/api/v1/runs/{id}/board")).await;
    let (_, at_end) = s
        .json(&format!("/api/v1/runs/{id}/board?upto={last}"))
        .await;
    assert_eq!(all, at_end);
    let b = &all["board"];
    assert_eq!(b["upto"], json!(last));
    assert_eq!(b["header"]["runtime"]["state"], json!("stopped"));
    assert_eq!(b["header"]["closed"], json!(true));
    assert_eq!(b["header"]["model"]["value"], json!(MODEL));
    assert_eq!(b["header"]["loops"]["demo"]["value"]["completed"], json!(3));
    assert_eq!(all["run"]["state"], json!("closed"));
    let tone = |id: &str| {
        b["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["node_id"] == json!(id))
            .unwrap()["tone"]
            .clone()
    };
    assert_eq!(tone("tool:lab/write_file"), json!("green"));
    assert_eq!(tone("action:demo/hold"), Value::Null, "legal, never run");
    assert_eq!(tone(&format!("runtime:{SANDBOX}")), json!("green"));
    // Mid-run: the board at the first tool call has write_marker running.
    let sel = pages
        .iter()
        .find(|e| e["kind"] == json!("action.selected"))
        .unwrap()["seq"]
        .as_u64()
        .unwrap();
    let (_, mid) = s.json(&format!("/api/v1/runs/{id}/board?upto={sel}")).await;
    let mid_tone = mid["board"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["node_id"] == json!("action:demo/write_marker"))
        .unwrap()
        .clone();
    assert_eq!(
        (mid_tone["tone"].clone(), mid_tone["latest"].clone()),
        (json!("amber"), json!(true))
    );
    assert_eq!(mid["board"]["edges"][0]["kind"], json!("chooses"));
}

/// A reload reads the same thing: runs, graph, events (any page size, views
/// included), the board at a `seq` and a fresh stream are byte-identical
/// on every read.
#[tokio::test]
async fn reload_returns_identical_order() {
    let s = Studio::start().await;
    let run = s.sink(Some("host:21:4f0e0d0c-0b0a-4908-8706-050403020100"));
    tick(&run, 1);
    tick(&run, 2);
    let id = run.run_id().unwrap().to_string();
    emit(&s.sink(None), 3, 0);
    let text = |path: String| {
        let req = s.get(&path);
        async move { req.send().await.unwrap().text().await.unwrap() }
    };
    for path in [
        "/api/v1/runs".to_string(),
        "/api/v1/graph".into(),
        format!("/api/v1/runs/{id}/events?limit=1000"),
        format!("/api/v1/runs/{id}/board"),
        format!("/api/v1/runs/{id}/board?upto=6"),
        format!("/api/v1/nodes/{}", "jev:demo"),
    ] {
        assert_eq!(text(path.clone()).await, text(path.clone()).await, "{path}");
    }
    // Each event keeps its trace line's field order (`tengu trace show`).
    let raw = text(format!("/api/v1/runs/{id}/events?limit=2")).await;
    assert!(
        raw.contains(r#""events":[{"schema_version":1,"event_id":""#),
        "{raw}"
    );
    let whole = replay(&s, &id, 1000).await;
    for limit in [1, 3, 7] {
        assert_eq!(replay(&s, &id, limit).await, whole, "limit {limit}");
    }
    // The board lists every graph node in graph order, every time.
    let (_, b) = s.json(&format!("/api/v1/runs/{id}/board?upto=6")).await;
    let (_, g) = s.json("/api/v1/graph").await;
    let order = |v: &Value, k: &str| {
        v.as_array()
            .unwrap()
            .iter()
            .map(|n| n[k].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        order(&b["board"]["nodes"], "node_id"),
        order(&g["nodes"], "id")
    );
    assert_eq!(b["board"]["upto"], json!(6));
    // Two fresh streams of the run: the same ids in the same order.
    let mut seen = Vec::new();
    for _ in 0..2 {
        let mut st = s.sse(&format!("/api/v1/runs/{id}/stream"), &[]).await;
        let mut ids = Vec::new();
        while ids.len() < whole.len() {
            ids.push(st.frame().await.id.unwrap());
        }
        seen.push(ids);
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(
        seen[0],
        whole
            .iter()
            .map(|e| e["event_id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    );
}

/// A restart is a new run: each run's events, board and state are its own
/// (ids, runtime, config); the old run is closed and flagged when its
/// config differs; the live stream follows only the new holder's run.
#[tokio::test]
async fn runs_are_not_mixed_across_restarts() {
    let s = Studio::with(|st| st.limits.holder_poll = Duration::from_millis(50)).await;
    let (h1, h2) = (
        "host:30:5f0e0d0c-0b0a-4908-8706-050403020100",
        "host:31:6f0e0d0c-0b0a-4908-8706-050403020100",
    );
    let older = "e".repeat(64);
    let old = sink_with_hash(&s, &older, Some(h1));
    tick(&old, 1);
    stop(&old);
    let new = s.sink(Some(h2));
    tick(&new, 1);
    s.beat(h2);
    let decide = s.sink(None);
    let root = decide
        .emit(
            EventDraft::new(Component::Loop, "trigger.decide", Status::Running)
                .node("trigger:decide"),
        )
        .unwrap();
    decide.emit(
        EventDraft::new(Component::Loop, "trigger.completed", Status::Ok)
            .node("trigger:decide")
            .parent(root),
    );
    let (old_id, new_id, decide_id) = (
        old.run_id().unwrap().to_string(),
        new.run_id().unwrap().to_string(),
        decide.run_id().unwrap().to_string(),
    );

    let (_, runs) = s.json("/api/v1/runs").await;
    let row = |id: &str| {
        runs["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["run_id"] == json!(id))
            .unwrap()
            .clone()
    };
    assert_eq!(runs["runs"].as_array().unwrap().len(), 3);
    assert_eq!(
        (
            row(&old_id)["state"].clone(),
            row(&old_id)["config_current"].clone()
        ),
        (json!("closed"), json!(false))
    );
    assert_eq!(
        (
            row(&new_id)["state"].clone(),
            row(&new_id)["config_current"].clone()
        ),
        (json!("live"), json!(true))
    );
    assert_eq!(row(&decide_id)["state"], json!("closed"));
    assert_eq!(runs["live_run_id"], json!(new_id));

    for (id, runtime, hash, state) in [
        (&old_id, json!(h1), json!(older), "closed"),
        (&new_id, json!(h2), json!(s.config_hash), "live"),
        (&decide_id, Value::Null, json!(s.config_hash), "closed"),
    ] {
        let evs = replay(&s, id, 1000).await;
        assert!(!evs.is_empty());
        for e in &evs {
            assert_eq!(e["run_id"], json!(id));
            assert_eq!(e["runtime_id"], runtime);
            assert_eq!(e["config_hash"], hash);
            assert!(e["event_id"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{id}:")));
        }
        let (_, b) = s.json(&format!("/api/v1/runs/{id}/board")).await;
        assert_eq!(b["run"]["state"], json!(state), "{id}");
        assert_eq!(b["board"]["header"]["run_id"], json!(id));
        for n in b["board"]["nodes"].as_array().unwrap() {
            if let Some(eid) = n["mark"]["event_id"].as_str() {
                assert!(eid.starts_with(&format!("{id}:")), "{eid} in {id}");
            }
        }
        for e in b["board"]["edges"].as_array().unwrap() {
            assert!(e["event_id"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{id}:")));
        }
    }
    let (_, ob) = s.json(&format!("/api/v1/runs/{old_id}/board")).await;
    assert_eq!(ob["run"]["config_current"], json!(false));
    assert_eq!(ob["board"]["header"]["runtime"]["state"], json!("stopped"));
    let (_, nb) = s.json(&format!("/api/v1/runs/{new_id}/board")).await;
    assert_eq!(nb["board"]["header"]["closed"], json!(false));
    assert_eq!(nb["board"]["header"]["runtime_id"], json!(h2));

    // The live stream: the new holder's run, from seq 1, nothing of the old.
    let mut live = s.sse("/api/v1/live/stream", &[]).await;
    let first = loop {
        let f = live.frame().await;
        if f.event == "run" && f.data["run_id"] != Value::Null {
            break f;
        }
    };
    assert_eq!(first.data["run_id"], json!(new_id));
    for want in 1..=10 {
        let f = live.frame().await;
        assert_eq!((f.seq(), f.data["run_id"].clone()), (want, json!(new_id)));
    }
}
