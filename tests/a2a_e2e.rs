//! A2A end to end: two `tengu` processes talk A2A over loopback — no
//! network, no API key. The server sandbox runs `tengu a2a serve` (its
//! planner at `/a2a`, the agent `helper` at `/a2a/agents/helper`, both on
//! `engine = "local"` against a scripted OpenAI-compatible mock); the client
//! sandbox calls it with `tengu a2a send` and through the `a2a` tool
//! (`tengu tool call`, the executor a `run-agent` step builds).
//!
//! | Test | Proves |
//! |---|---|
//! | `exposed_agent_answers_v1_and_v03` | a v1.0 send (JSON-RPC, bearer token) runs the agent with its tools and returns a `COMPLETED` task whose artifact is the model's answer; the same agent through its 0.3 card (`agent.json`) answers in 0.3 form; a follow-up in the context hands the agent the earlier turn |
//! | `planner_front_door_answers` | `/a2a` runs a one-shot orchestrator turn (the planner's prose reply becomes the answer) |
//! | `the_a2a_tool_calls_the_remote` | the opt-in `a2a` tool: `list`, `card`, `send` against the server, scoped (`net_hosts`, `env_reads`) |
//! | `a_wrong_token_is_refused` | the server answers 401 to a wrong bearer token; the client names it |
//! | `serve_refuses_without_its_token` | `token_env` unset ⇒ `tengu a2a serve` exits non-zero naming the variable |
//! | `a_brief_leads_the_request` | `[agents.<a>.brief]`: the searcher agent gets the request as is, before the agent; what it found leads the agent's request in a marked block; `NONE` adds nothing; the context's history keeps the requests without briefs; a searcher that fails leaves the turn as it was |

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// What the scripted model answers, every time.
const ANSWER: &str = "A2A-E2E-ANSWER-7f3c9a: the helper heard you.";
const TOKEN: &str = "e2e-bearer-token-0123456789abcdef";

/// In a searcher's identity: the mock answers it as the searcher.
const SEARCHER_ROLE: &str = "BRIEF-SEARCHER-ROLE";
/// What the mock searcher finds.
const FOUND: &str = "BRIEF-FACT-41d2: the user is allergic to penicillin (Health.md:5-5).";
/// In a request: the mock searcher finds nothing there.
const NOTHING_THERE: &str = "NOTHING-THERE";

/// OpenAI-compatible mock: each request answered by a rule (by default
/// [`ANSWER`]); the request bodies kept.
struct Llm {
    url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Llm {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn read_body(sock: &mut std::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = sock.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return String::new();
        }
        buf.extend_from_slice(&chunk[..n]);
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map_or(0, |v| v.trim().parse().unwrap_or(0));
        while buf.len() < end + 4 + len {
            let n = sock.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        return String::from_utf8_lossy(&buf[end + 4..]).into_owned();
    }
}

fn llm() -> Llm {
    llm_with(|_| ANSWER.to_string())
}

/// The mock for briefs: a request whose system prompt holds
/// [`SEARCHER_ROLE`] is the searcher's — [`FOUND`], or `NONE` when the
/// request says [`NOTHING_THERE`]; any other request gets [`ANSWER`].
fn searcher_llm() -> Llm {
    llm_with(|body| {
        let texts = |role: &str| -> String {
            body["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| m["role"] == role)
                .filter_map(|m| m["content"].as_str())
                .collect()
        };
        if !texts("system").contains(SEARCHER_ROLE) {
            ANSWER.to_string()
        } else if texts("user").contains(NOTHING_THERE) {
            "NONE".to_string()
        } else {
            FOUND.to_string()
        }
    })
}

fn llm_with(rule: fn(&Value) -> String) -> Llm {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (b, s) = (Arc::clone(&bodies), Arc::clone(&stop));
    std::thread::spawn(move || {
        while !s.load(Ordering::SeqCst) {
            let Ok((mut sock, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };
            sock.set_nonblocking(false).ok();
            sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
            let body: Value = serde_json::from_str(&read_body(&mut sock)).unwrap_or(Value::Null);
            let content = rule(&body);
            b.lock().unwrap().push(body);
            let reply = json!({"choices": [{"message": {"role": "assistant", "content": content},
                               "finish_reason": "stop"}]})
            .to_string();
            let _ = write!(
                sock,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    Llm { url, bodies, stop }
}

impl Llm {
    /// Every message text of every request so far.
    fn seen(&self) -> String {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .flat_map(|b| b["messages"].as_array().cloned().unwrap_or_default())
            .filter_map(|m| m["content"].as_str().map(str::to_string))
            .collect::<Vec<_>>()
            .join("\n---\n")
    }

    /// Each request so far as (system prompt, user messages in order).
    fn requests(&self) -> Vec<(String, Vec<String>)> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .map(|b| {
                let msgs = b["messages"].as_array().cloned().unwrap_or_default();
                let of = |role: &str| -> Vec<String> {
                    msgs.iter()
                        .filter(|m| m["role"] == role)
                        .filter_map(|m| m["content"].as_str().map(str::to_string))
                        .collect()
                };
                (of("system").concat(), of("user"))
            })
            .collect()
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `tengu a2a serve` on its sandbox; killed when dropped.
struct Server {
    child: Child,
    port: u16,
    _dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn server_toml(llm: &str, port: u16, ws: &Path) -> String {
    format!(
        r#"
[egress]
network = "open"
audit = false

[orchestrator]
agent = "planner"

[agents.planner]
engine = "local"
model = "mock"
default = true

[agents.planner.local]
base_url = "{llm}"

[agents.planner.limits]
context_window = 16384

[agents.helper]
engine = "local"
model = "mock"
description = "Answers questions for other agent harnesses."
workspace = "{ws}"
tools = ["read_file"]

[agents.helper.local]
base_url = "{llm}"

[agents.helper.limits]
context_window = 16384

[a2a.server]
port = {port}
token_env = "TENGU_A2A_E2E_TOKEN"
orchestrator = true
agents = ["helper"]
"#,
        ws = ws.display()
    )
}

fn tengu(dir: &Path, config: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tengu"));
    cmd.arg("-c")
        .arg(config)
        .current_dir(dir)
        .env("TENGU_HOME", dir.join("home"))
        .env_remove("TENGU_CONFIG")
        .env_remove("TENGU_EGRESS")
        .env_remove("TENGU_SESSION_ID")
        .env_remove("TENGU_AGENT_NAME")
        .env_remove("OPENROUTER_API_KEY");
    cmd
}

fn serve(llm: &Llm) -> Server {
    serve_toml(|port, ws| server_toml(&llm.url, port, ws))
}

/// `tengu a2a serve` on the config `toml(port, workspace)` writes.
fn serve_toml(toml: impl Fn(u16, &Path) -> String) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let port = free_port();
    let config = dir.path().join("server.toml");
    std::fs::write(&config, toml(port, &ws)).unwrap();
    let child = tengu(dir.path(), &config)
        .args(["a2a", "serve"])
        .env("TENGU_A2A_E2E_TOKEN", TOKEN)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tengu a2a serve");
    let server = Server {
        child,
        port,
        _dir: dir,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let ok = std::net::TcpStream::connect(("127.0.0.1", port))
            .and_then(|mut s| {
                s.write_all(b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")?;
                let mut out = String::new();
                s.read_to_string(&mut out)?;
                Ok(out.starts_with("HTTP/1.1 200"))
            })
            .unwrap_or(false);
        if ok {
            return server;
        }
        assert!(
            Instant::now() < deadline,
            "tengu a2a serve did not come up on {port}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The client sandbox: remotes for the server's endpoints, the `a2a` tool.
struct Client {
    dir: tempfile::TempDir,
    config: PathBuf,
}

fn client(llm: &Llm, port: u16) -> Client {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("client.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[egress]
network = "open"
audit = false

[agents.asker]
engine = "local"
model = "mock"
default = true
tools = ["a2a"]

[agents.asker.local]
base_url = "{llm}"

[agents.asker.limits]
context_window = 16384

[default_scopes.a2a]
net_hosts = ["127.0.0.1"]
env_reads = ["TENGU_A2A_E2E_TOKEN"]

[a2a.remotes.helper]
url = "http://127.0.0.1:{port}/a2a/agents/helper"
description = "The other sandbox's helper agent."
bearer_env = "TENGU_A2A_E2E_TOKEN"
timeout_secs = 60

[a2a.remotes.helper03]
url = "http://127.0.0.1:{port}/a2a/agents/helper/.well-known/agent.json"
bearer_env = "TENGU_A2A_E2E_TOKEN"
timeout_secs = 60

[a2a.remotes.front]
url = "http://127.0.0.1:{port}"
bearer_env = "TENGU_A2A_E2E_TOKEN"
timeout_secs = 60
"#,
            llm = llm.url
        ),
    )
    .unwrap();
    Client { dir, config }
}

impl Client {
    /// `tengu a2a <args>` with `token`; (exit ok, stdout, stderr).
    fn a2a(&self, token: &str, args: &[&str]) -> (bool, String, String) {
        let out = tengu(self.dir.path(), &self.config)
            .arg("a2a")
            .args(args)
            .env("TENGU_A2A_E2E_TOKEN", token)
            .output()
            .expect("run tengu a2a");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// `tengu a2a send --json`; the result JSON.
    fn send(&self, remote: &str, text: &str, extra: &[&str]) -> Value {
        let mut args = vec!["send", "--remote", remote, text, "--json"];
        args.extend_from_slice(extra);
        let (ok, stdout, stderr) = self.a2a(TOKEN, &args);
        assert!(ok, "tengu a2a send failed:\n{stdout}\n{stderr}");
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"))
    }
}

fn artifact_text(task: &Value) -> String {
    task["artifacts"][0]["parts"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[test]
fn exposed_agent_answers_v1_and_v03() {
    let llm = llm();
    let server = serve(&llm);
    let c = client(&llm, server.port);

    let r = c.send("helper", "first question from the other harness", &[]);
    let task = &r["task"];
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{r}");
    assert_eq!(artifact_text(task), ANSWER);
    assert!(llm.seen().contains("first question from the other harness"));

    // A follow-up in the same context: the agent gets the earlier turn.
    let ctx = task["contextId"].as_str().unwrap().to_string();
    let r2 = c.send("helper", "second question", &["--context", &ctx]);
    assert_eq!(r2["task"]["contextId"], ctx.as_str());
    let last = llm.bodies.lock().unwrap().last().cloned().unwrap();
    let msgs: Vec<&str> = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect();
    let first_at = msgs
        .iter()
        .position(|m| m.contains("first question"))
        .expect("history");
    let second_at = msgs
        .iter()
        .position(|m| m.contains("second question"))
        .unwrap();
    assert!(first_at < second_at, "{msgs:?}");

    // The 0.3 card → the 0.3 dialect, the same agent.
    let (ok, stdout, stderr) = c.a2a(TOKEN, &["card", "--remote", "helper03"]);
    assert!(ok, "{stdout}{stderr}");
    assert!(stdout.contains("interface: JsonRpc 0.3 at"), "{stdout}");
    let r3 = c.send("helper03", "old client question", &[]);
    assert_eq!(
        r3["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{r3}"
    );
    assert_eq!(artifact_text(&r3["task"]), ANSWER);
}

#[test]
fn planner_front_door_answers() {
    let llm = llm();
    let server = serve(&llm);
    let c = client(&llm, server.port);
    let (ok, stdout, _) = c.a2a(TOKEN, &["card", "--remote", "front"]);
    assert!(ok);
    assert!(
        stdout.contains("- helper (helper): Answers questions"),
        "{stdout}"
    );
    let r = c.send("front", "route this please", &[]);
    assert_eq!(r["task"]["status"]["state"], "TASK_STATE_COMPLETED", "{r}");
    assert!(
        artifact_text(&r["task"]).contains("A2A-E2E-ANSWER-7f3c9a"),
        "{r}"
    );
}

#[test]
fn the_a2a_tool_calls_the_remote() {
    let llm = llm();
    let server = serve(&llm);
    let c = client(&llm, server.port);
    let call = |args: Value| -> Value {
        let out = tengu(c.dir.path(), &c.config)
            .args([
                "tool", "call", "--agent", "asker", "--tool", "a2a", "--args",
            ])
            .arg(args.to_string())
            .env("TENGU_A2A_E2E_TOKEN", TOKEN)
            .output()
            .expect("run tengu tool call");
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{e}: {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
    };
    let list = call(json!({"action": "list"}));
    assert_eq!(list["is_error"], false, "{list}");
    assert!(
        list["text"]
            .as_str()
            .unwrap()
            .contains("- helper  http://127.0.0.1:"),
        "{list}"
    );
    let card = call(json!({"action": "card", "remote": "helper"}));
    assert!(
        card["text"]
            .as_str()
            .unwrap()
            .contains("a2a helper: helper v"),
        "{card}"
    );
    let sent = call(json!({"action": "send", "remote": "helper", "message": "tool question"}));
    assert_eq!(sent["is_error"], false, "{sent}");
    let text = sent["text"].as_str().unwrap();
    assert!(text.contains("TASK_STATE_COMPLETED"), "{text}");
    assert!(text.contains(ANSWER), "{text}");
    let unknown = call(json!({"action": "send", "remote": "nope", "message": "x"}));
    assert_eq!(unknown["is_error"], true, "{unknown}");
}

#[test]
fn a_wrong_token_is_refused() {
    let llm = llm();
    let server = serve(&llm);
    let c = client(&llm, server.port);
    let (ok, stdout, stderr) = c.a2a("wrong-token", &["send", "--remote", "helper", "hi"]);
    assert!(!ok, "{stdout}");
    assert!(stderr.contains("HTTP 401"), "{stderr}");
}

#[test]
fn serve_refuses_without_its_token() {
    let llm = llm();
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("server.toml");
    std::fs::write(&config, server_toml(&llm.url, free_port(), dir.path())).unwrap();
    let out = tengu(dir.path(), &config)
        .args(["a2a", "serve"])
        .env_remove("TENGU_A2A_E2E_TOKEN")
        .output()
        .expect("run tengu a2a serve");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("TENGU_A2A_E2E_TOKEN is unset or empty"),
        "{stderr}"
    );
}

/// The server for briefs: `briefed` gets its brief from `finder` (a
/// searcher by its identity), `unlucky` from `deaf`, whose model server is
/// gone.
fn brief_toml(llm: &str, port: u16, ws: &Path, gone: &str) -> String {
    format!(
        r#"
[egress]
network = "open"
audit = false

[agents.briefed]
engine = "local"
model = "mock"
default = true
description = "Answers with what the user's knowledge base says."
workspace = "{ws}"
tools = ["read_file"]

[agents.briefed.local]
base_url = "{llm}"

[agents.briefed.limits]
context_window = 16384

[agents.briefed.brief]
searcher = "finder"

[agents.finder]
engine = "local"
model = "mock"
tools = ["read_file"]

[agents.finder.identity]
instructions = "{SEARCHER_ROLE}: find what the knowledge base says about the request, or answer NONE."

[agents.finder.local]
base_url = "{llm}"

[agents.finder.limits]
context_window = 16384

[agents.unlucky]
engine = "local"
model = "mock"
description = "Same, but its searcher's model server is gone."
tools = ["read_file"]

[agents.unlucky.local]
base_url = "{llm}"

[agents.unlucky.limits]
context_window = 16384

[agents.unlucky.brief]
searcher = "deaf"
timeout_secs = 20

[agents.deaf]
engine = "local"
model = "mock"

[agents.deaf.local]
base_url = "{gone}"

[agents.deaf.limits]
context_window = 16384

[a2a.server]
port = {port}
token_env = "TENGU_A2A_E2E_TOKEN"
agents = ["briefed", "unlucky"]
"#,
        ws = ws.display()
    )
}

#[test]
fn a_brief_leads_the_request() {
    let llm = searcher_llm();
    let gone = format!("http://127.0.0.1:{}/v1", free_port());
    let server = serve_toml(|port, ws| brief_toml(&llm.url, port, ws, &gone));
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("client.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[agents.asker]
engine = "local"
model = "mock"
default = true

[agents.asker.local]
base_url = "{llm}"

[a2a.remotes.briefed]
url = "http://127.0.0.1:{port}/a2a/agents/briefed"
bearer_env = "TENGU_A2A_E2E_TOKEN"
timeout_secs = 60

[a2a.remotes.unlucky]
url = "http://127.0.0.1:{port}/a2a/agents/unlucky"
bearer_env = "TENGU_A2A_E2E_TOKEN"
timeout_secs = 60
"#,
            llm = llm.url,
            port = server.port
        ),
    )
    .unwrap();
    let c = Client { dir, config };
    let searcher = |r: &(String, Vec<String>)| r.0.contains(SEARCHER_ROLE);

    // The searcher runs first, on the request as is; its finding leads the
    // agent's request.
    let request = "Prescribe an antibiotic for my sore throat.";
    let r = c.send("briefed", request, &[]);
    assert_eq!(r["task"]["status"]["state"], "TASK_STATE_COMPLETED", "{r}");
    assert_eq!(artifact_text(&r["task"]), ANSWER);
    let turns = llm.requests();
    assert_eq!(turns.len(), 2, "the searcher, then the agent: {turns:?}");
    assert!(searcher(&turns[0]), "{turns:?}");
    assert_eq!(turns[0].1, [request], "the searcher gets the request alone");
    assert!(!searcher(&turns[1]), "{turns:?}");
    assert_eq!(
        turns[1].1,
        [format!(
            "[Brief from agent `finder`: what the user's knowledge base says that matters for this request]\n\
             {FOUND}\n\
             [End of brief]\n\n\
             {request}"
        )]
    );

    // A follow-up where the searcher finds nothing: no brief; the history
    // holds the first request without its brief.
    let ctx = r["task"]["contextId"].as_str().unwrap().to_string();
    let follow = format!("And for the cough? {NOTHING_THERE}");
    c.send("briefed", &follow, &["--context", &ctx]);
    let turns = llm.requests();
    assert_eq!(turns.len(), 4, "{turns:?}");
    assert!(searcher(&turns[2]) && !searcher(&turns[3]), "{turns:?}");
    assert_eq!(
        turns[2].1,
        [follow.as_str()],
        "the searcher gets no history"
    );
    assert_eq!(turns[3].1, [request, follow.as_str()]);

    // A searcher that fails: the turn goes on with the request as it was.
    let lone = "Which vaccines are due?";
    let r = c.send("unlucky", lone, &[]);
    assert_eq!(r["task"]["status"]["state"], "TASK_STATE_COMPLETED", "{r}");
    assert_eq!(artifact_text(&r["task"]), ANSWER);
    let turns = llm.requests();
    assert_eq!(turns.len(), 5, "the deaf searcher never reached the mock");
    assert_eq!(turns[4].1, [lone]);
}
