//! Engine matrix (`x-engine-matrix-smoke`, milestone E0, tracker convention
//! 20): one scripted turn through `tengu run-agent` per engine × model × tool
//! set, on the fixture sandboxes in `tests/fixtures/engine_matrix/` — one
//! agent per engine × model; the IPC `compose` hands each leg exactly its
//! tool set (as a planner step with `Step.compose` does).
//!
//! | Tool set | Scripted calls | The leg also asserts |
//! |---|---|---|
//! | `workspace` | `list_directory` `.` → `read_file` the `token-*` file → `write_file` `answer.txt` = the token → `read_file` `second.txt` | the answer holds the token (its file name is only in the listing, its value only in the file); `answer.txt` = the token; `second.txt` holds a registered secret (`TENGU_SECRETS_LOADED`): the answer quotes `REDACTED`; Claude Code: the bridge's result for it, logged by the engine, is `[REDACTED]` |
//! | `hyperliquid` | `hl_ctx` `{"coins": ["xyz:TSLA"]}` → `hl_book` `{"coin": "xyz:TSLA"}` — live, read-only | the answer holds a number of the stored `mkt_ctx/1:hyperliquid:xyz:TSLA` headline and one of `hl_book/1:hyperliquid:xyz:TSLA` |
//! | `xm` | `risk_status` on a new paper account (`[xmarket]` + `[risk]` + `[paper]`, ledger in a temp `TENGU_HOME`) | the answer holds `86.42` (its `equity=`: the fixtures' `initial_cash_usd`); `ledger.db` is under that `TENGU_HOME` |
//!
//! Every leg: exit 0, `status = ok`, every tool of the set in the IPC `tools`
//! activity and no run of it failed (Claude Code: the bridged calls the
//! engine reports, `StreamEvent::ToolRan`); the secret's value is nowhere in
//! the child's stdout or stderr. The fixtures' configured scopes exclude the
//! workspace, so every call also proves the `run-agent` workspace grant — for
//! Claude Code the bridge's (`TENGU_AGENT_IPC=1`). Children run in the leg's
//! workspace (no repo `.env`, no `TENGU_PLAN.md`), without `HL_API_URL`
//! (mainnet) and without a parent Claude Code session's env.
//!
//! | Engine · model | Fixture · agent | Tests | Needs |
//! |---|---|---|---|
//! | openrouter · `google/gemini-2.5-flash-lite` | `openrouter.toml` · `gemini` | `openrouter_gemini_*` | `OPENROUTER_API_KEY` (env or the repo's `.env`) |
//! | openrouter · `anthropic/claude-haiku-4.5` | `openrouter.toml` · `haiku` | `openrouter_haiku_*` | same |
//! | claude_code · `claude-haiku-4-5`, built-ins off | `claude_code.toml` · `claude` | `claude_code_*` | `--features claude_code`, `claude` logged in (subscription) |
//! | local · `gemma4:latest` | `local.toml` · `gemma` | `local_*` | `TENGU_MATRIX_LOCAL_BASE_URL`; unset ⇒ skipped; loopback on macOS ⇒ skipped (local models run on the operator's PC) |
//! | local → a scripted OpenAI-compatible mock | `local.toml` · `gemma` | `offline_local_workspace`, `offline_local_xm` (not ignored) | nothing |
//! | — | all three | `fixtures_load_and_agree` (not ignored) | nothing |
//!
//! Live run (sequential; one `engine_matrix |` result line per leg):
//! `cargo test --features claude_code --test engine_matrix -- --ignored --nocapture --test-threads 1`

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/engine_matrix");
/// Registered for redaction (`TENGU_SECRETS_LOADED`) in every leg.
const SECRET_VAR: &str = "TENGU_MATRIX_SECRET";
/// Holds the secret's value. An innocuous name: a model echoes what it
/// read instead of withholding a "secret" on its own.
const SECRET_FILE: &str = "second.txt";
/// `risk_status` `equity=` of a new account: the fixtures' `[paper]
/// initial_cash_usd`.
const EQUITY: &str = "86.42";
/// The fixtures' `[xmarket] state`.
const XM_STATE: &str = "engine-matrix";
/// Env a parent Claude Code session sets (when these tests run from one): a
/// nested `claude` must start like one from the operator's terminal.
const PARENT_SESSION_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
];

// ---------------------------------------------------------------------------
// Matrix
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    OpenRouter,
    ClaudeCode,
    Local,
}

/// One engine × model: its fixture and agent.
#[derive(Clone, Copy)]
struct Target {
    kind: Kind,
    fixture: &'static str,
    agent: &'static str,
    label: &'static str,
}

const GEMINI: Target = Target {
    kind: Kind::OpenRouter,
    fixture: "openrouter.toml",
    agent: "gemini",
    label: "openrouter google/gemini-2.5-flash-lite",
};
const HAIKU: Target = Target {
    kind: Kind::OpenRouter,
    fixture: "openrouter.toml",
    agent: "haiku",
    label: "openrouter anthropic/claude-haiku-4.5",
};
const CLAUDE: Target = Target {
    kind: Kind::ClaudeCode,
    fixture: "claude_code.toml",
    agent: "claude",
    label: "claude_code claude-haiku-4-5",
};
const GEMMA: Target = Target {
    kind: Kind::Local,
    fixture: "local.toml",
    agent: "gemma",
    label: "local gemma4:latest",
};
/// `GEMMA` against the offline mock server.
const MOCK: Target = Target {
    label: "local mock server",
    ..GEMMA
};

#[derive(Clone, Copy)]
enum Set {
    Workspace,
    Hyperliquid,
    Xm,
}

impl Set {
    const ALL: [Set; 3] = [Set::Workspace, Set::Hyperliquid, Set::Xm];

    fn name(self) -> &'static str {
        match self {
            Set::Workspace => "workspace",
            Set::Hyperliquid => "hyperliquid",
            Set::Xm => "xm",
        }
    }

    fn tools(self) -> &'static [&'static str] {
        match self {
            Set::Workspace => &["list_directory", "read_file", "write_file"],
            Set::Hyperliquid => &["hl_ctx", "hl_book"],
            Set::Xm => &["risk_status"],
        }
    }

    /// The scripted goal: numbered steps, one tool call each, then a
    /// one-line answer that can only come from the results.
    fn goal(self) -> String {
        let (steps, answer): (&[&str], &str) = match self {
            Set::Workspace => (
                &[
                    "Call list_directory with path \".\". Exactly one file name starts with \"token-\".",
                    "Call read_file on that file. Its whole content is the token.",
                    "Call write_file with path \"answer.txt\" and content exactly the token.",
                    "Call read_file on \"second.txt\".",
                ],
                "the token, then the exact text read_file returned for second.txt",
            ),
            Set::Hyperliquid => (
                &[
                    "Call hl_ctx with {\"coins\": [\"xyz:TSLA\"]}.",
                    "Call hl_book with {\"coin\": \"xyz:TSLA\"}.",
                ],
                "the mark= value hl_ctx returned and the bid= value hl_book returned, exactly as printed",
            ),
            Set::Xm => (
                &["Call risk_status with no arguments."],
                "the account and the equity= value risk_status returned, exactly as printed",
            ),
        };
        let steps: Vec<String> = steps
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{}. {s}", i + 1))
            .collect();
        format!(
            "Scripted tool test. Use your tools (they may be listed as mcp__tengu-tools__<name>) \
             and do these steps in order, one tool call each:\n{}\n\
             Then reply with exactly one line: {answer}. If you call compress_and_store, use that \
             line as its summary.",
            steps.join("\n")
        )
    }
}

/// One `#[ignore]` live test per engine × model × tool set.
macro_rules! live_legs {
    ($($name:ident => $target:expr, $set:expr;)*) => {$(
        #[test]
        #[ignore = "live: calls a model (module doc: what each engine needs)"]
        fn $name() {
            live_leg($target, $set);
        }
    )*};
}

live_legs! {
    openrouter_gemini_workspace => GEMINI, Set::Workspace;
    openrouter_gemini_hyperliquid => GEMINI, Set::Hyperliquid;
    openrouter_gemini_xm => GEMINI, Set::Xm;
    openrouter_haiku_workspace => HAIKU, Set::Workspace;
    openrouter_haiku_hyperliquid => HAIKU, Set::Hyperliquid;
    openrouter_haiku_xm => HAIKU, Set::Xm;
    claude_code_workspace => CLAUDE, Set::Workspace;
    claude_code_hyperliquid => CLAUDE, Set::Hyperliquid;
    claude_code_xm => CLAUDE, Set::Xm;
    local_workspace => GEMMA, Set::Workspace;
    local_hyperliquid => GEMMA, Set::Hyperliquid;
    local_xm => GEMMA, Set::Xm;
}

fn live_leg(target: Target, set: Set) {
    let (envs, timeout) = match target.kind {
        Kind::OpenRouter => {
            let key =
                openrouter_key().expect("OPENROUTER_API_KEY is not set (env or the repo's .env)");
            (vec![("OPENROUTER_API_KEY", key)], 300)
        }
        Kind::ClaudeCode => {
            assert!(
                cfg!(feature = "claude_code"),
                "build with --features claude_code (the tengu binary runs the engine)"
            );
            // The engine logs each bridged tool result at debug: the
            // bridge's own answer for `SECRET_FILE`.
            let log = "tengu::adapters::outbound::engines::claude_code=debug";
            (vec![("RUST_LOG", log.to_string())], 600)
        }
        Kind::Local => match local_base_url() {
            Ok(url) => (vec![("TENGU_MATRIX_LOCAL_BASE_URL", url)], 1_200),
            Err(why) => {
                println!("engine_matrix | {} | {} | {why}", target.label, set.name());
                return;
            }
        },
    };
    let ws = workspace();
    let leg = run_leg(target, set, &ws, &envs, Duration::from_secs(timeout));
    assert_leg(target, set, &leg, &ws);
}

/// `OPENROUTER_API_KEY` from the env, else from the repo's `.env` (read
/// here, handed to the child only; never printed).
fn openrouter_key() -> Option<String> {
    std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .or_else(|| {
            dotenvy::from_path_iter(Path::new(env!("CARGO_MANIFEST_DIR")).join(".env"))
                .ok()?
                .flatten()
                .find(|(k, _)| k == "OPENROUTER_API_KEY")
                .map(|(_, v)| v)
                .filter(|v| !v.is_empty())
        })
}

/// The live local legs' server: `TENGU_MATRIX_LOCAL_BASE_URL` — the
/// operator's PC over the LAN, never a loopback server on macOS (operator
/// rule 2026-09-30: no local model on the Mac). `Err` = why the leg skips.
fn local_base_url() -> Result<String, &'static str> {
    let url = std::env::var("TENGU_MATRIX_LOCAL_BASE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .ok_or("skipped (TENGU_MATRIX_LOCAL_BASE_URL is not set)")?;
    if cfg!(target_os = "macos") && is_loopback(&url) {
        return Err("skipped (local models run on the operator's PC)");
    }
    Ok(url)
}

/// `url` names this machine (the rule of `tengu doctor --engines`).
fn is_loopback(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    let host = url
        .host_str()
        .unwrap_or("")
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

// ---------------------------------------------------------------------------
// One leg
// ---------------------------------------------------------------------------

/// A leg's workspace (token file, decoy, [`SECRET_FILE`]) and `TENGU_HOME`.
struct Workspace {
    _dir: tempfile::TempDir,
    path: PathBuf,
    _home: tempfile::TempDir,
    home: PathBuf,
    token_file: String,
    token: String,
    secret: String,
}

fn workspace() -> Workspace {
    let dir = tempfile::tempdir().expect("workspace");
    // Canonical: scope checks compare resolved paths (macOS /var → /private/var).
    let path = std::fs::canonicalize(dir.path()).expect("canonical workspace");
    let home = tempfile::tempdir().expect("TENGU_HOME");
    let token = format!("matrix-{}", uuid::Uuid::new_v4().simple());
    let token_file = format!("token-{}.txt", uuid::Uuid::new_v4().simple());
    let secret = format!("matrix-second-{}", uuid::Uuid::new_v4().simple());
    std::fs::write(path.join(&token_file), &token).unwrap();
    std::fs::write(path.join("notes.txt"), "not the token\n").unwrap();
    std::fs::write(path.join(SECRET_FILE), &secret).unwrap();
    Workspace {
        _dir: dir,
        path,
        home: home.path().to_path_buf(),
        _home: home,
        token_file,
        token,
        secret,
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(FIXTURES).join(name)
}

/// What one `tengu run-agent` leg produced.
struct Leg {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    ipc: Value,
    secs: f64,
}

impl Leg {
    /// `output` + `summary` — a model may answer in either.
    fn answer(&self) -> String {
        format!(
            "{}\n{}",
            self.ipc["output"].as_str().unwrap_or(""),
            self.ipc["summary"].as_str().unwrap_or("")
        )
    }

    /// IPC `tools`: `(name, ok)` in call order.
    fn runs(&self) -> Vec<(String, bool)> {
        self.ipc["tools"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|r| {
                        (
                            r["name"].as_str().unwrap_or("").to_string(),
                            r["ok"].as_bool().unwrap_or(false),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Prompt / completion tokens summed over the child's turns.
    fn tokens(&self) -> (u64, u64) {
        let sum = |k: &str| {
            self.ipc["metrics"]
                .as_array()
                .map(|m| m.iter().filter_map(|r| r[k].as_u64()).sum())
                .unwrap_or(0)
        };
        (sum("prompt_tokens"), sum("completion_tokens"))
    }

    /// The Claude CLI's `total_cost_usd` (API-price equivalent; the
    /// subscription is not billed per token), from the child's log.
    fn cli_cost_usd(&self) -> Option<f64> {
        let re = regex::Regex::new(r"cost_usd=([0-9.]+)").unwrap();
        let costs: Vec<f64> = re
            .captures_iter(&self.stderr)
            .filter_map(|c| c[1].parse().ok())
            .collect();
        (!costs.is_empty()).then(|| costs.iter().sum())
    }

    fn context(&self) -> String {
        format!(
            "exit {:?}\n--- stdout ---\n{}\n--- stderr (tail) ---\n{}",
            self.code,
            self.stdout,
            tail(&self.stderr, 6_000)
        )
    }
}

fn tail(s: &str, n: usize) -> &str {
    let mut start = s.len().saturating_sub(n);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Run `tengu run-agent` for `target` over `set`, like `SubprocessRunner`
/// does (`TENGU_AGENT_IPC=1`, one JSON on stdin; `compose` = the set), with
/// `envs` on top and a watchdog.
fn run_leg(
    target: Target,
    set: Set,
    ws: &Workspace,
    envs: &[(&str, String)],
    timeout: Duration,
) -> Leg {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tengu"));
    cmd.arg("run-agent")
        .current_dir(&ws.path)
        .env("TENGU_AGENT_IPC", "1")
        .env("TENGU_HOME", &ws.home)
        .env("TENGU_CONFIG", fixture(target.fixture))
        .env("TENGU_MATRIX_WORKSPACE", &ws.path)
        .env("TENGU_SECRETS_LOADED", SECRET_VAR)
        .env(SECRET_VAR, &ws.secret)
        .env_remove("TENGU_EGRESS")
        .env_remove("TENGU_SESSION_ID")
        .env_remove("TENGU_AGENT_NAME")
        .env_remove("HL_API_URL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for k in PARENT_SESSION_ENV {
        cmd.env_remove(k);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let input = json!({
        "goal": set.goal(),
        "agent_name": target.agent,
        "model": "",
        "max_turns": 10,
        "session_id": format!("engine-matrix-{}", uuid::Uuid::new_v4().simple()),
        "step_id": format!("matrix-{}", set.name()),
        "compose": {"base_agent": target.agent, "skills": [], "tools": set.tools()},
    });
    let started = Instant::now();
    let mut child = cmd.spawn().expect("spawn tengu run-agent");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.to_string().as_bytes())
        .expect("write IPC input");
    let mut out_pipe = child.stdout.take().expect("stdout");
    let mut err_pipe = child.stderr.take().expect("stderr");
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        out_pipe.read_to_string(&mut s).ok();
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        err_pipe.read_to_string(&mut s).ok();
        s
    });
    let status = loop {
        if let Some(st) = child.try_wait().expect("try_wait") {
            break Some(st);
        }
        if started.elapsed() > timeout {
            child.kill().ok();
            child.wait().ok();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let secs = started.elapsed().as_secs_f64();
    let stdout = out.join().unwrap();
    // The child's log without its colours: `cost_usd=…`, readable panics.
    let ansi = regex::Regex::new("\x1b\\[[0-9;]*m").unwrap();
    let stderr = ansi.replace_all(&err.join().unwrap(), "").into_owned();
    let Some(status) = status else {
        panic!(
            "run-agent ({}) did not exit within {timeout:?}\n--- stderr (tail) ---\n{}",
            target.label,
            tail(&stderr, 6_000)
        );
    };
    let ipc = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
    Leg {
        code: status.code(),
        stdout,
        stderr,
        ipc,
        secs,
    }
}

/// Print the leg's result line, then assert what every leg shares and what
/// its tool set reads (module doc).
fn assert_leg(target: Target, set: Set, leg: &Leg, ws: &Workspace) {
    let runs = leg.runs();
    let called: Vec<String> = runs
        .iter()
        .map(|(n, ok)| if *ok { n.clone() } else { format!("{n}:error") })
        .collect();
    let (tin, tout) = leg.tokens();
    let cost = leg
        .cli_cost_usd()
        .map_or(String::new(), |c| format!(" | cli cost_usd={c:.4}"));
    println!(
        "engine_matrix | {} | {} | status={} | {:.1}s | tokens in/out {tin}/{tout}{cost} | tools: {}",
        target.label,
        set.name(),
        leg.ipc["status"].as_str().unwrap_or("-"),
        leg.secs,
        called.join(", ")
    );
    let label = format!("{} {}", target.label, set.name());
    assert_eq!(leg.code, Some(0), "{label}\n{}", leg.context());
    assert_eq!(leg.ipc["status"], "ok", "{label}\n{}", leg.context());
    for tool in set.tools() {
        assert!(
            runs.iter().any(|(n, _)| n == tool),
            "{label}: {tool} was not called; tools: {called:?}\n{}",
            leg.context()
        );
        assert!(
            !runs.iter().any(|(n, ok)| n == tool && !ok),
            "{label}: {tool} returned an error; tools: {called:?}\n{}",
            leg.context()
        );
    }
    for (what, text) in [("stdout", &leg.stdout), ("stderr", &leg.stderr)] {
        assert!(
            !text.contains(&ws.secret),
            "{label}: the secret leaked in clear to {what}"
        );
    }
    let answer = leg.answer();
    match set {
        Set::Workspace => {
            let written = std::fs::read_to_string(ws.path.join("answer.txt")).unwrap_or_default();
            assert_eq!(
                written.trim(),
                ws.token,
                "{label}: answer.txt does not hold the token\n{}",
                leg.context()
            );
            assert!(
                answer.contains(&ws.token),
                "{label}: the answer does not hold the token (result not read)\n{}",
                leg.context()
            );
            let reads = runs.iter().filter(|(n, _)| n == "read_file").count();
            assert!(reads >= 2, "{label}: {SECRET_FILE} not read: {called:?}");
            // Models may drop the brackets; the word only comes from the result.
            assert!(
                answer.contains("REDACTED"),
                "{label}: the answer should quote the redacted {SECRET_FILE}\n{}",
                leg.context()
            );
            if matches!(target.kind, Kind::ClaudeCode) {
                assert!(
                    leg.stderr
                        .lines()
                        .any(|l| l.contains("Claude Code tool result") && l.contains("[REDACTED]")),
                    "{label}: no bridged tool result `[REDACTED]` in the engine's log\n{}",
                    leg.context()
                );
            }
        }
        Set::Hyperliquid => {
            for key in [
                "mkt_ctx/1:hyperliquid:xyz:TSLA",
                "hl_book/1:hyperliquid:xyz:TSLA",
            ] {
                let headline = stored_headline(&ws.path, key)
                    .unwrap_or_else(|| panic!("{label}: no stored {key} row\n{}", leg.context()));
                assert!(
                    quotes_any(&answer, &headline_numbers(&headline)),
                    "{label}: the answer quotes no number of {key} `{headline}`\n{}",
                    leg.context()
                );
            }
        }
        Set::Xm => {
            assert!(
                answer.contains(EQUITY),
                "{label}: the answer does not hold equity {EQUITY}\n{}",
                leg.context()
            );
            let ledger = ws.home.join("state").join(XM_STATE).join("ledger.db");
            assert!(ledger.exists(), "{label}: no {}", ledger.display());
        }
    }
}

/// `headline` of the row `key` in the workspace observation store.
fn stored_headline(workspace: &Path, key: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(workspace.join(".tengu/observations.db")).ok()?;
    let body: String = conn
        .query_row("SELECT body FROM observations WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .ok()?;
    let row: Value = serde_json::from_str(&body).ok()?;
    row["headline"].as_str().map(str::to_string)
}

/// Values of a headline's `key=value` pairs with ≥ 3 significant digits
/// (prices and sizes, not counts or small ratios a model could guess).
fn headline_numbers(headline: &str) -> Vec<f64> {
    let re = regex::Regex::new(r"=([-+]?\d+(?:\.\d+)?)").unwrap();
    re.captures_iter(headline)
        .filter(|c| {
            c[1].chars()
                .filter(char::is_ascii_digit)
                .skip_while(|d| *d == '0')
                .count()
                >= 3
        })
        .filter_map(|c| c[1].parse().ok())
        .collect()
}

/// `text` holds one of `numbers` (numerically: `347.10` = `347.1`).
fn quotes_any(text: &str, numbers: &[f64]) -> bool {
    let re = regex::Regex::new(r"\d+(?:\.\d+)?").unwrap();
    let quoted: Vec<f64> = re
        .find_iter(text)
        .filter_map(|m| m.as_str().parse().ok())
        .collect();
    numbers.iter().any(|n| {
        quoted
            .iter()
            .any(|q| (q - n).abs() <= 1e-9 * n.abs().max(1.0))
    })
}

// ---------------------------------------------------------------------------
// Offline: the local path end to end against a scripted OpenAI-compatible mock
// ---------------------------------------------------------------------------

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

/// OpenAI-compatible mock on a random loopback port: answers `replies` in
/// order (one connection each) and returns the request bodies; gives up
/// after 20 s.
fn mock_server(replies: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
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
    (url, handle)
}

fn tool_call_reply(id: &str, name: &str, args: &Value) -> String {
    json!({"choices": [{"message": {"content": null, "tool_calls": [{
        "id": id, "type": "function",
        "function": {"name": name, "arguments": args.to_string()}
    }]}, "finish_reason": "tool_calls"}]})
    .to_string()
}

fn text_reply(text: &str) -> String {
    json!({"choices": [{"message": {"content": text}, "finish_reason": "stop"}]}).to_string()
}

/// Tool messages of one request body, in order.
fn tool_messages(body: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(body).expect("request JSON");
    v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or("").to_string())
        .collect()
}

/// Tool names advertised in one request body.
fn advertised(body: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(body).expect("request JSON");
    v["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn offline_leg(set: Set, ws: &Workspace, replies: Vec<String>) -> (Leg, Vec<String>) {
    let (url, server) = mock_server(replies);
    let leg = run_leg(
        MOCK,
        set,
        ws,
        &[("TENGU_MATRIX_LOCAL_BASE_URL", url)],
        Duration::from_secs(25),
    );
    let bodies = server.join().expect("mock server");
    (leg, bodies)
}

/// The workspace set on the local engine, scripted: every call runs through
/// the fixture's configured scopes + the run-agent grant, each result
/// reaches the model (the listing names the token file, the read returns the
/// token, the secret comes back redacted), and the model sees only the
/// composed set.
#[test]
fn offline_local_workspace() {
    let ws = workspace();
    let (leg, bodies) = offline_leg(
        Set::Workspace,
        &ws,
        vec![
            tool_call_reply("c1", "list_directory", &json!({"path": "."})),
            tool_call_reply("c2", "read_file", &json!({"path": ws.token_file})),
            tool_call_reply(
                "c3",
                "write_file",
                &json!({"path": "answer.txt", "content": ws.token}),
            ),
            tool_call_reply("c4", "read_file", &json!({"path": SECRET_FILE})),
            text_reply(&format!("{} [REDACTED]", ws.token)),
        ],
    );
    assert_leg(MOCK, Set::Workspace, &leg, &ws);
    let ran: Vec<(String, bool)> = ["list_directory", "read_file", "write_file", "read_file"]
        .iter()
        .map(|t| (t.to_string(), true))
        .collect();
    assert_eq!(leg.runs(), ran);
    assert_eq!(bodies.len(), 5, "{}", leg.context());
    let mut names = advertised(&bodies[0]);
    names.sort();
    assert_eq!(
        names,
        [
            "compress_and_store",
            "list_directory",
            "read_file",
            "write_file"
        ]
    );
    let results = tool_messages(&bodies[4]);
    assert!(results[0].contains(&ws.token_file), "{results:?}");
    assert!(results[1].contains(&ws.token), "{results:?}");
    assert!(
        results[3].contains("[REDACTED]") && !results[3].contains(&ws.secret),
        "{results:?}"
    );
}

/// The xm set on the local engine, scripted: `[xmarket]` + `[risk]` +
/// `[paper]` load, `risk_status` opens a new account in the leg's
/// `TENGU_HOME` and its row reaches the model.
#[test]
fn offline_local_xm() {
    let ws = workspace();
    let (leg, bodies) = offline_leg(
        Set::Xm,
        &ws,
        vec![
            tool_call_reply("c1", "risk_status", &json!({})),
            text_reply(&format!("matrix equity={EQUITY}")),
        ],
    );
    assert_leg(MOCK, Set::Xm, &leg, &ws);
    assert_eq!(bodies.len(), 2, "{}", leg.context());
    let results = tool_messages(&bodies[1]);
    assert!(
        results[0].contains(&format!("risk account=matrix halt=none equity={EQUITY}")),
        "{results:?}"
    );
}

/// The three fixtures share every section but `[agents.*]`, every agent
/// holds all three tool sets, and each fixture loads in tengu: its agents
/// run `risk_status` in-process (`tengu tool call`) — no model involved.
#[test]
fn fixtures_load_and_agree() {
    let mut shared: Option<toml::Table> = None;
    for target in [GEMINI, CLAUDE, GEMMA] {
        let text = std::fs::read_to_string(fixture(target.fixture)).unwrap();
        let mut table: toml::Table = toml::from_str(&text).unwrap();
        let agents = table.remove("agents").expect("[agents.*]");
        for (name, agent) in agents.as_table().unwrap() {
            let tools: Vec<&str> = agent["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|t| t.as_str())
                .collect();
            let all: Vec<&str> = Set::ALL.iter().flat_map(|s| s.tools()).copied().collect();
            assert_eq!(tools, all, "{}: agents.{name}.tools", target.fixture);
        }
        match &shared {
            None => shared = Some(table),
            Some(first) => assert_eq!(
                first, &table,
                "{}: sections other than [agents.*] differ from openrouter.toml",
                target.fixture
            ),
        }
    }
    for target in [GEMINI, HAIKU, CLAUDE, GEMMA] {
        let ws = workspace();
        let out = Command::new(env!("CARGO_BIN_EXE_tengu"))
            .args([
                "tool",
                "call",
                "--agent",
                target.agent,
                "--tool",
                "risk_status",
            ])
            .arg("-c")
            .arg(fixture(target.fixture))
            .current_dir(&ws.path)
            .env("TENGU_HOME", &ws.home)
            .env("TENGU_MATRIX_WORKSPACE", &ws.path)
            .env_remove("TENGU_CONFIG")
            .env_remove("TENGU_EGRESS")
            .env_remove("TENGU_SECRETS_LOADED")
            .output()
            .expect("spawn tengu tool call");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let result: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
        assert!(
            result["is_error"] == false
                && result["text"]
                    .as_str()
                    .is_some_and(|t| t.contains(&format!("equity={EQUITY}"))),
            "{} as {}: {stdout}\n{}",
            target.fixture,
            target.agent,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
