//! Black-box test of the `tengu run-agent` IPC boundary.
//!
//! Replaces `scripts/test-runner.sh`. Drives the built binary the same way
//! `SubprocessRunner` does — `TENGU_AGENT_IPC=1`, one `AgentIpcInput` JSON on
//! stdin, cwd = repo root — through the pre-LLM failure paths and one
//! `engine = "local"` loop against a loopback fake server, so no network /
//! API key is needed.
//!
//! | Case | Trigger | Expected (from `src/inbound/cli/run_agent.rs::run_agent_subprocess`) |
//! |---|---|---|
//! | guard | no `TENGU_AGENT_IPC` | exit != 0, stderr: "subprocess mode not meant for direct invocation" |
//! | bad json | `TENGU_AGENT_IPC=1`, stdin = `not json` | exit != 0, stderr: "parse IPC input JSON", stdout empty |
//! | no agent | valid input, `agent_name = "__no_such_agent__"` | exit != 0, stderr: "no agent `__no_such_agent__` in the active config", stdout empty |
//! | local fit | local agent, 16 384-token window, `read_file` of 100 000 bytes | the tool message the server gets next is ≤ 8 192 bytes (1/8 of the window), exit 0; a local agent left on the default window is warned about on stderr |
//!
//! None of the failure paths emit an `AgentIpcOutput` — every failure before
//! the tool loop propagates as `anyhow::Error` out of `main`, and the parent
//! (`src/adapters/outbound/subprocess_runner.rs`, non-zero-status check) turns that into a step
//! failure without parsing stdout. The tests assert exactly that contract.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PER_TEST_TIMEOUT: Duration = Duration::from_secs(5);

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Spawn `tengu run-agent`, feed `stdin` (then close it), and wait with a
/// watchdog so a hang shows up as a test failure instead of a stuck runner.
fn run_agent(ipc_env: Option<&str>, stdin_payload: &str) -> Run {
    run_agent_with(ipc_env, stdin_payload, &[], PER_TEST_TIMEOUT)
}

/// [`run_agent`] with extra env vars (e.g. `TENGU_CONFIG`) and a timeout.
fn run_agent_with(
    ipc_env: Option<&str>,
    stdin_payload: &str,
    envs: &[(&str, &str)],
    timeout: Duration,
) -> Run {
    let tengu_home = tempfile::tempdir().expect("tempdir for TENGU_HOME");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tengu"));
    cmd.arg("run-agent")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("TENGU_HOME", tengu_home.path())
        .env_remove("TENGU_AGENT_IPC")
        .env_remove("TENGU_CONFIG")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("TENGU_SESSION_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(v) = ipc_env {
        cmd.env("TENGU_AGENT_IPC", v);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().expect("spawn tengu run-agent");

    // Write the payload and drop the handle so the child sees EOF
    // (`run_agent_subprocess` reads stdin to end before parsing).
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(stdin_payload.as_bytes())
            .expect("write IPC payload");
    }

    // Drain pipes on threads so a chatty child can't deadlock on a full pipe
    // while we poll for exit.
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let out_thread = std::thread::spawn(move || {
        let mut s = String::new();
        stdout_pipe.read_to_string(&mut s).ok();
        s
    });
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        stderr_pipe.read_to_string(&mut s).ok();
        s
    });

    let status = wait_with_timeout(&mut child, timeout);
    let stdout = out_thread.join().expect("stdout reader thread");
    let stderr = err_thread.join().expect("stderr reader thread");

    let code = match status {
        Some(st) => st.code(),
        None => panic!(
            "tengu run-agent did not exit within {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            timeout, stdout, stderr
        ),
    };

    Run {
        code,
        stdout,
        stderr,
    }
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    loop {
        if let Some(st) = child.try_wait().expect("try_wait") {
            return Some(st);
        }
        if start.elapsed() > timeout {
            child.kill().ok();
            child.wait().ok();
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn valid_input(agent_name: &str) -> String {
    serde_json::json!({
        "goal": "say hello and confirm IPC works",
        "agent_name": agent_name,
        "model": "openai/gpt-4o",
        "tools": ["http_request"],
        "skills": ["web-research"],
        "max_turns": 5,
        "sandbox": null,
        "session_id": "test-session-run-agent-ipc",
        "step_id": "step-1"
    })
    .to_string()
}

fn assert_no_ipc_json(run: &Run) {
    assert!(
        run.stdout.trim().is_empty(),
        "expected empty stdout (no AgentIpcOutput before the tool loop), got:\n{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("\"status\""),
        "stdout must not carry an IPC status object:\n{}",
        run.stdout
    );
}

#[test]
fn run_agent_refuses_without_ipc_env() {
    let run = run_agent(None, &valid_input("researcher"));

    assert_ne!(
        run.code,
        Some(0),
        "expected non-zero exit; stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("`tengu run-agent` is a subprocess mode not meant for direct invocation"),
        "stderr should carry the IPC guard message, got:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("TENGU_AGENT_IPC=1"),
        "stderr should tell the operator which env var to set, got:\n{}",
        run.stderr
    );
    assert_no_ipc_json(&run);
}

#[test]
fn run_agent_rejects_invalid_json_input() {
    let run = run_agent(Some("1"), "this is not json {");

    assert_ne!(
        run.code,
        Some(0),
        "expected non-zero exit; stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("parse IPC input JSON"),
        "stderr should carry the JSON parse context, got:\n{}",
        run.stderr
    );
    assert_no_ipc_json(&run);
}

#[test]
fn run_agent_fails_fast_on_unknown_agent() {
    let run = run_agent(Some("1"), &valid_input("__no_such_agent__"));

    assert_ne!(
        run.code,
        Some(0),
        "expected non-zero exit; stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("no agent `__no_such_agent__` in the active config"),
        "stderr should name the unknown agent, got:\n{}",
        run.stderr
    );
    // The bail happens before engine construction, so no API key is consulted.
    assert!(
        !run.stderr.contains("OPENROUTER_API_KEY"),
        "unknown-agent path must fail before engine build, got:\n{}",
        run.stderr
    );
    assert_no_ipc_json(&run);
}

/// Request body of one HTTP/1.1 request (headers, then `Content-Length`).
fn read_request_body(sock: &mut std::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16_384];
    loop {
        let n = sock.read(&mut chunk).expect("read request");
        if n == 0 {
            return String::new();
        }
        buf.extend_from_slice(&chunk[..n]);
        let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map_or(0, |v| v.trim().parse().expect("content-length"));
        let body = head_end + 4;
        if buf.len() >= body + len {
            return String::from_utf8_lossy(&buf[body..body + len]).into_owned();
        }
    }
}

/// OpenAI-compatible fake: answers `replies` in order (one connection
/// each) and returns the request bodies; gives up after 20 s.
fn fake_local_server(replies: Vec<&'static str>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut bodies = Vec::new();
        for reply in replies {
            let mut sock = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() > deadline {
                            return bodies;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            };
            sock.set_nonblocking(false).unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            bodies.push(read_request_body(&mut sock));
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.len()
            );
            sock.write_all(head.as_bytes()).expect("write head");
            sock.write_all(reply.as_bytes()).expect("write body");
        }
        bodies
    });
    (port, handle)
}

/// `x-local-model-fit`: a local agent with a 16 384-token window never gets
/// a tool result above 1/8 of it (8 192 bytes, footer included), and
/// `Config::load` warns about a local agent left on the default window.
#[test]
fn local_engine_child_fits_tool_results_to_the_window() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("big.txt"), "z".repeat(100_000)).unwrap();
    let (port, server) = fake_local_server(vec![
        r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"big.txt\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        r#"{"choices":[{"message":{"content":"done"},"finish_reason":"stop"}]}"#,
    ]);
    let config = format!(
        "[egress]\nnetwork = \"open\"\n\n[memory]\nenabled = false\n\n\
         [agents.fitter]\nengine = \"local\"\nmodel = \"gemma4:latest\"\n\
         description = \"fixture: local-model fit\"\ntools = [\"read_file\"]\n\
         workspace = \"{workspace}\"\n\n\
         [agents.fitter.limits]\ncontext_window = 16384\n\n\
         [agents.fitter.local]\nbase_url = \"http://127.0.0.1:{port}/v1\"\napi_key_env = \"\"\n\n\
         [agents.unsized]\nengine = \"local\"\nmodel = \"gemma4:latest\"\n",
        workspace = workspace.path().display()
    );
    let config_path = workspace.path().join("config.toml");
    std::fs::write(&config_path, config).unwrap();
    let input = serde_json::json!({
        "goal": "read big.txt",
        "agent_name": "fitter",
        "model": "",
        "max_turns": 3,
        "session_id": "test-session-local-fit",
        "step_id": "step-1"
    })
    .to_string();

    let run = run_agent_with(
        Some("1"),
        &input,
        &[("TENGU_CONFIG", config_path.to_str().unwrap())],
        Duration::from_secs(20),
    );
    let bodies = server.join().expect("fake server");
    assert_eq!(
        run.code,
        Some(0),
        "stdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    assert_eq!(bodies.len(), 2, "stderr:\n{}", run.stderr);
    let second: serde_json::Value = serde_json::from_str(&bodies[1]).expect("request JSON");
    let messages = second["messages"].as_array().unwrap();
    let systems = messages.iter().filter(|m| m["role"] == "system").count();
    assert_eq!(systems, 1, "system prompt sent once");
    let tool = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("tool message in round 2");
    let content = tool["content"].as_str().unwrap();
    assert!(content.len() <= 8_192, "{} bytes > 8192", content.len());
    assert!(
        content.ends_with(" of 100000 chars]"),
        "footer missing: {}",
        &content[content.len().saturating_sub(80)..]
    );
    assert!(
        run.stderr
            .contains("agents.unsized.limits.context_window is the 1000000 default"),
        "load warning missing:\n{}",
        run.stderr
    );
}

/// Sanity: the wire shape the tests send is what `SubprocessRunner` sends.
/// Guards against the fixture drifting from `AgentIpcInput` required fields.
#[test]
fn fixture_has_required_ipc_fields() {
    let v: serde_json::Value = serde_json::from_str(&valid_input("researcher")).unwrap();
    for key in ["goal", "agent_name", "model", "session_id", "step_id"] {
        assert!(
            v.get(key).is_some(),
            "fixture missing required field `{key}`"
        );
    }
}
