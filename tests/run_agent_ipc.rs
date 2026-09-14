//! Black-box test of the `tengu run-agent` IPC boundary.
//!
//! Replaces `scripts/test-runner.sh`. Drives the built binary the same way
//! `SubprocessRunner` does — `TENGU_AGENT_IPC=1`, one `AgentIpcInput` JSON on
//! stdin, cwd = repo root so `agents/<name>.toml` resolves — but only through
//! the pre-LLM failure paths so no network / API key is needed.
//!
//! | Case | Trigger | Expected (from `src/main.rs::run_agent_subprocess`) |
//! |---|---|---|
//! | guard | no `TENGU_AGENT_IPC` | exit != 0, stderr: "subprocess mode not meant for direct invocation" |
//! | bad json | `TENGU_AGENT_IPC=1`, stdin = `not json` | exit != 0, stderr: "parse IPC input JSON", stdout empty |
//! | no agent | valid input, `agent_name = "__no_such_agent__"` | exit != 0, stderr: "load agent spec from agents/__no_such_agent__.toml", stdout empty |
//!
//! None of these paths emit an `AgentIpcOutput` — every failure before the
//! tool loop propagates as `anyhow::Error` out of `main`, and the parent
//! (`src/adapters/runner.rs`, non-zero-status check) turns that into a step
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
    let tengu_home = tempfile::tempdir().expect("tempdir for TENGU_HOME");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tengu"));
    cmd.arg("run-agent")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("TENGU_HOME", tengu_home.path())
        .env_remove("TENGU_AGENT_IPC")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("TENGU_SESSION_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(v) = ipc_env {
        cmd.env("TENGU_AGENT_IPC", v);
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

    let status = wait_with_timeout(&mut child, PER_TEST_TIMEOUT);
    let stdout = out_thread.join().expect("stdout reader thread");
    let stderr = err_thread.join().expect("stderr reader thread");

    let code = match status {
        Some(st) => st.code(),
        None => panic!(
            "tengu run-agent did not exit within {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            PER_TEST_TIMEOUT, stdout, stderr
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
            .contains("load agent spec from agents/__no_such_agent__.toml"),
        "stderr should name the missing agent spec path, got:\n{}",
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
