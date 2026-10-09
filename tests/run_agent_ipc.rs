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
//! | widening compose | hardened sandbox (a `[solana]` signer), `compose.tools` adds `write_file` to a base listing `read_file` | exit != 0 before the engine is built, stderr: "compose would widen agent `arch` in a hardened sandbox", stdout empty; the same compose without the signer offers `write_file` to a loopback model, exit 0 |
//! | local fit | local agent, 16 384-token window, `read_file` of 100 000 bytes | the tool message the server gets next is ≤ 8 192 bytes (1/8 of the window), exit 0; a local agent left on the default window is warned about on stderr |
//! | OpenRouter caps | `OPENROUTER_BASE_URL` = a loopback fake, `max_tool_result_chars = 2000`, `compact_result_limit = 50`, `read_file` of 100 011 bytes, then a small file | round 0's result capped (+ footer); in the next request it is line 1 only; one system message per request |
//! | Claude Code, no workspace (`--features claude_code`) | `[claude_code] cli_path` = a stand-in that logs argv, cwd and stdin, streams a `compress_and_store` call, writes the step's summary file, then sleeps 30 s | `--mcp-config` + every tool in `--allowedTools`; cwd = a `tengu-step-*` temp dir, gone after the step; the system prompt only in `--system-prompt`; the bridge transcript = system, goal, the call; the run ends at the stored summary (< 15 s), IPC `summary` = the stub's |
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

/// W1-gate review: in a hardened sandbox (here a `[solana]` signer; `[risk]`
/// takes the same path) a plan step's `compose` may only narrow its base
/// agent. Adding `write_file` to a base that lists `read_file` fails the
/// step before the engine is built (no IPC output). Without the signer the
/// same compose replaces the tools wholesale (doctrine #3): the loopback
/// model is offered `write_file`.
#[test]
fn run_agent_refuses_a_widening_compose_in_a_hardened_sandbox() {
    let workspace = tempfile::tempdir().expect("workspace");
    let config_dir = tempfile::tempdir().expect("config dir");
    let (port, server) = fake_local_server(vec![
        r#"{"choices":[{"message":{"content":"done"},"finish_reason":"stop"}]}"#,
    ]);
    let input = serde_json::json!({
        "goal": "write the answer",
        "agent_name": "arch",
        "model": "",
        "max_turns": 2,
        "session_id": "test-session-hardened-compose",
        "step_id": "step-1",
        "compose": {"base_agent": "arch", "skills": [], "tools": ["read_file", "write_file"]}
    })
    .to_string();
    let run_with = |signer: &str| {
        let config = format!(
            "[egress]\nnetwork = \"open\"\n\n[memory]\nenabled = false\n\n{signer}\
             [agents.arch]\nengine = \"local\"\nmodel = \"gemma4:latest\"\n\
             description = \"fixture: hardened compose\"\ntools = [\"read_file\"]\n\
             workspace = \"{}\"\n\n\
             [agents.arch.limits]\ncontext_window = 16384\n\n\
             [agents.arch.local]\nbase_url = \"http://127.0.0.1:{port}/v1\"\napi_key_env = \"\"\n",
            workspace.path().display()
        );
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, config).unwrap();
        run_agent_with(
            Some("1"),
            &input,
            &[("TENGU_CONFIG", config_path.to_str().unwrap())],
            Duration::from_secs(20),
        )
    };

    let run = run_with(
        "[solana]\nprivy_wallet_id = \"w1\"\n[keys]\nproxy = \"http://127.0.0.1:9\"\n\
         agent_socket = \"/nonexistent\"\nstrip = [\"PRIVY_APP_SECRET\"]\n\
         [keys.env]\nPRIVY_API_URL = \"privy\"\n\n",
    );
    assert_ne!(run.code, Some(0), "stderr:\n{}", run.stderr);
    assert!(
        run.stderr
            .contains("compose would widen agent `arch` in a hardened sandbox")
            && run.stderr.contains("[\"write_file\"]"),
        "stderr should name the widening, got:\n{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("subprocess engine built"),
        "the refusal comes before the engine build, got:\n{}",
        run.stderr
    );
    assert_no_ipc_json(&run);

    let open = run_with("");
    assert_eq!(open.code, Some(0), "stderr:\n{}", open.stderr);
    let bodies = server.join().expect("fake server");
    assert_eq!(bodies.len(), 1, "only the open run calls the model");
    let request: serde_json::Value = serde_json::from_str(&bodies[0]).expect("request JSON");
    let offered: Vec<&str> = request["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(
        offered.contains(&"write_file") && offered.contains(&"read_file"),
        "not hardened: compose replaces the tools wholesale, offered {offered:?}"
    );
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

/// W1-gate parity: a run-agent step on OpenRouter gets every tool result
/// capped at `limits.max_tool_result_chars` and older rounds compacted to
/// line 1 (`compact_result_limit`), as in-process chat does — a 100 KB file
/// is not resent whole on every later turn; the system prompt goes out once.
#[test]
fn openrouter_step_caps_results_and_compacts_older_rounds() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("big.txt"),
        format!("first line\n{}", "z".repeat(100_000)),
    )
    .unwrap();
    std::fs::write(workspace.path().join("small.txt"), "small").unwrap();
    let (port, server) = fake_local_server(vec![
        r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"big.txt\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"c2","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"small.txt\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        r#"{"choices":[{"message":{"content":"done"},"finish_reason":"stop"}]}"#,
    ]);
    let config = format!(
        "[egress]\nnetwork = \"open\"\n\n[memory]\nenabled = false\n\n\
         [agents.capped]\nengine = \"openrouter\"\nmodel = \"test/model\"\n\
         description = \"fixture: result caps\"\ntools = [\"read_file\"]\n\
         workspace = \"{workspace}\"\n\n\
         [agents.capped.limits]\nmax_tool_result_chars = 2000\ncompact_result_limit = 50\n",
        workspace = workspace.path().display()
    );
    let config_path = workspace.path().join("config.toml");
    std::fs::write(&config_path, config).unwrap();
    let input = serde_json::json!({
        "goal": "read big.txt then small.txt",
        "agent_name": "capped",
        "model": "",
        "max_turns": 4,
        "session_id": "test-session-openrouter-caps",
        "step_id": "step-1"
    })
    .to_string();

    let run = run_agent_with(
        Some("1"),
        &input,
        &[
            ("TENGU_CONFIG", config_path.to_str().unwrap()),
            ("OPENROUTER_API_KEY", "test-key"),
            ("OPENROUTER_BASE_URL", &format!("http://127.0.0.1:{port}")),
        ],
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
    assert_eq!(bodies.len(), 3, "stderr:\n{}", run.stderr);
    let requests: Vec<serde_json::Value> = bodies
        .iter()
        .map(|b| serde_json::from_str(b).expect("request JSON"))
        .collect();
    for (i, r) in requests.iter().enumerate() {
        let systems = r["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "system")
            .count();
        assert_eq!(systems, 1, "request {i}: the system prompt goes out once");
    }
    let tools = |r: &serde_json::Value| -> Vec<String> {
        r["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| m["content"].as_str().unwrap().to_string())
            .collect()
    };
    let second = tools(&requests[1]);
    assert!(
        second[0].starts_with("first line\nzzz")
            && second[0].ends_with("[truncated — showing 2000 of 100011 chars]"),
        "round 0's result capped: {} bytes, tail {:?}",
        second[0].len(),
        &second[0][second[0].len().saturating_sub(60)..]
    );
    assert!(second[0].len() < 2_100, "{} bytes", second[0].len());
    let third = tools(&requests[2]);
    assert_eq!(
        third,
        ["first line", "small"],
        "an older round keeps line 1, the latest stays"
    );
}

/// W1-gate parity: a `claude_code` plan step whose agent has no `workspace`
/// (e.g. a `researcher` agent) runs in a temp dir of its own — the CLI's cwd and
/// the bridge's workspace — and gets the bridge (`--mcp-config`, its tools
/// in `--allowedTools`, `compress_and_store` included); the system prompt
/// rides on `--system-prompt` only; the bridge's transcript holds the
/// step's messages and the streamed call; a stored summary ends the CLI run
/// (the stand-in would sleep 30 s); the temp dir goes with the step.
#[cfg(feature = "claude_code")]
#[test]
fn claude_code_step_without_workspace_runs_in_a_temp_dir_with_the_bridge() {
    let dir = tempfile::tempdir().expect("stub dir");
    let log = dir.path().join("log");
    let stub = dir.path().join("claude");
    std::fs::write(
        &stub,
        r#"#!/bin/sh
LOG="$TENGU_IPC_STUB_LOG"
{ echo "cwd=$(pwd -P)"; for a in "$@"; do echo "arg=$a"; done; } > "$LOG.args"
cat > "$LOG.prompt"
CFG=""; prev=""
for a in "$@"; do [ "$prev" = "--mcp-config" ] && CFG="$a"; prev="$a"; done
cp "$CFG" "$LOG.mcp"
TRANSCRIPT=$(sed -n 's/.*"TENGU_BRIDGE_TRANSCRIPT_FILE":"\([^"]*\)".*/\1/p' "$CFG")
SUMMARY=$(sed -n 's/.*"TENGU_BRIDGE_SUMMARY_FILE":"\([^"]*\)".*/\1/p' "$CFG")
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"mcp__tengu-tools__compress_and_store","input":{"summary":"stub summary"}}]}}'
sleep 0.5
cp "$TRANSCRIPT" "$LOG.transcript"
printf 'stub summary' > "$SUMMARY"
echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"stored — stop now"}]}}'
exec sleep 30
"#,
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let config = format!(
        "[egress]\nnetwork = \"open\"\n\n[memory]\nenabled = false\n\n\
         [claude_code]\ncli_path = \"{stub}\"\n\n\
         [agents.main]\ndefault = true\nengine = \"openrouter\"\nmodel = \"x/y\"\n\n\
         [agents.researcher]\nengine = \"claude_code\"\nmodel = \"claude-haiku-4-5\"\n\
         tools = [\"http_request\", \"read_file\", \"list_directory\"]\n\
         description = \"fixture: no workspace\"\n",
        stub = stub.display()
    );
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config).unwrap();
    let goal = "what is the BTC price?";
    let input = serde_json::json!({
        "goal": goal,
        "agent_name": "researcher",
        "model": "",
        "max_turns": 3,
        "session_id": "test-session-cc-no-workspace",
        "step_id": "step-1"
    })
    .to_string();

    let started = Instant::now();
    let run = run_agent_with(
        Some("1"),
        &input,
        &[
            ("TENGU_CONFIG", config_path.to_str().unwrap()),
            ("TENGU_IPC_STUB_LOG", log.to_str().unwrap()),
        ],
        Duration::from_secs(25),
    );
    let took = started.elapsed();
    assert_eq!(
        run.code,
        Some(0),
        "stdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        took < Duration::from_secs(15),
        "the stored summary ended the run: {took:?}"
    );
    let out: serde_json::Value = serde_json::from_str(
        run.stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default(),
    )
    .expect("IPC output JSON");
    assert_eq!(out["status"], "ok", "{out}");
    assert_eq!(out["summary"], "stub summary", "{out}");

    let read = |suffix: &str| {
        std::fs::read_to_string(format!("{}{suffix}", log.display()))
            .unwrap_or_else(|e| panic!("{suffix}: {e}; stderr:\n{}", run.stderr))
    };
    let args = read(".args");
    for flag in [
        "arg=--mcp-config",
        "arg=--allowedTools",
        "arg=mcp__tengu-tools__http_request",
        "arg=mcp__tengu-tools__read_file",
        "arg=mcp__tengu-tools__list_directory",
        "arg=mcp__tengu-tools__compress_and_store",
        "arg=--system-prompt",
    ] {
        assert!(args.lines().any(|l| l == flag), "{flag} missing:\n{args}");
    }
    let cwd = args
        .lines()
        .find_map(|l| l.strip_prefix("cwd="))
        .expect("cwd logged");
    let temp_root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    assert!(
        std::path::Path::new(cwd).starts_with(&temp_root)
            && std::path::Path::new(cwd)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("tengu-step-"),
        "the CLI runs in the step's temp dir, not {cwd}"
    );
    assert!(
        !std::path::Path::new(cwd).exists(),
        "the step's temp dir goes with the step: {cwd}"
    );
    let mcp: serde_json::Value = serde_json::from_str(&read(".mcp")).unwrap();
    let env = &mcp["mcpServers"]["tengu-tools"]["env"];
    assert_eq!(env["TENGU_BRIDGE_WORKSPACE"], cwd, "{env}");
    assert_eq!(env["TENGU_BRIDGE_AGENT"], "researcher");

    let prompt = read(".prompt");
    assert!(prompt.contains(goal), "{prompt}");
    assert!(
        !prompt.contains("You are a focused subagent"),
        "the system prompt rides on --system-prompt only:\n{prompt}"
    );
    let transcript: Vec<serde_json::Value> =
        serde_json::from_str(&read(".transcript")).expect("transcript JSON");
    assert_eq!(transcript.len(), 3, "{transcript:?}");
    assert_eq!(transcript[0]["role"], "system");
    assert_eq!(
        (
            transcript[1]["role"].as_str(),
            transcript[1]["content"].as_str()
        ),
        (Some("user"), Some(goal))
    );
    assert_eq!(
        transcript[2]["tool_calls"][0]["name"], "compress_and_store",
        "{transcript:?}"
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
