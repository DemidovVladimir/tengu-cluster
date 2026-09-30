//! The real `tengu mcp-bridge`, driven over stdio with the env the Claude
//! Code engine writes into its `--mcp-config`.
//!
//! | Test | Env | Expected |
//! |---|---|---|
//! | `bridge_proxies_external_mcp_server_tools` | `TENGU_BRIDGE_TOOLS` naming a `{server}__{tool}` entry + `TENGU_BRIDGE_MCP_SERVERS` (`tests/fixtures/fake_mcp_server.sh`) | the external tool is listed and callable through the bridge |
//! | `bridge_runs_tools_as_the_configured_agent` | `TENGU_CONFIG` + `TENGU_BRIDGE_AGENT` naming an agent with a denying scope; `TENGU_SECRETS_LOADED` naming a var | the agent's scope applies (not the permissive fallback); the secret comes back redacted |

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

/// A running bridge + its stdio.
struct Bridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Bridge {
    fn spawn(envs: &[(&str, String)]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tengu"));
        cmd.arg("mcp-bridge")
            .env("TENGU_EGRESS", r#"{"network":"open","audit":false}"#)
            .env_remove("TENGU_AGENT_IPC")
            .env_remove("TENGU_BRIDGE_AGENT")
            .env_remove("TENGU_SECRETS_LOADED")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn tengu mcp-bridge");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn rpc(&mut self, id: u64, method: &str, params: Value) -> Value {
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{req}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad reply {line:?}: {e}"))
    }

    fn call(&mut self, id: u64, tool: &str, args: Value) -> Value {
        self.rpc(id, "tools/call", json!({"name": tool, "arguments": args}))["result"].clone()
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn tool(name: &str) -> Value {
    json!({"name": name, "description": "d", "parameters": {"type": "object", "properties": {}}})
}

#[test]
fn bridge_proxies_external_mcp_server_tools() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/fake_mcp_server.sh"
    );
    let workspace = tempfile::TempDir::new().unwrap();
    let servers = json!([{
        "name": "fake",
        "transport": "stdio",
        "command": ["sh", fixture]
    }]);

    let mut bridge = Bridge::spawn(&[
        (
            "TENGU_BRIDGE_WORKSPACE",
            workspace.path().display().to_string(),
        ),
        (
            "TENGU_BRIDGE_TOOLS",
            json!([tool("fake__echo")]).to_string(),
        ),
        ("TENGU_BRIDGE_MCP_SERVERS", servers.to_string()),
        (
            "TENGU_CONFIG",
            workspace.path().join("absent.toml").display().to_string(),
        ),
    ]);

    bridge.rpc(1, "initialize", json!({}));
    let listed = bridge.rpc(2, "tools/list", json!({}));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(names, ["fake__echo"], "tools/list: {listed}");

    let called = bridge.call(3, "fake__echo", json!({}));
    assert_eq!(called["content"][0]["text"], "pong", "tools/call: {called}");
}

#[test]
fn bridge_runs_tools_as_the_configured_agent() {
    const SECRET: &str = "sk-e2e-bridge-0123456789abcdef";
    let dir = tempfile::TempDir::new().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("notes.txt"), format!("token={SECRET}\n")).unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        r#"
[agents.main]
default = true
engine = "openrouter"
model = "anthropic/claude-haiku-4.5"

[agents.bridged]
engine = "claude_code"
model = "claude-haiku-4-5"

# Configured scope with no fs roots: listing is denied.
[agents.bridged.scopes.list_directory]
fs_roots = []
"#,
    )
    .unwrap();

    let mut bridge = Bridge::spawn(&[
        ("TENGU_HOME", dir.path().join("home").display().to_string()),
        ("TENGU_CONFIG", config.display().to_string()),
        ("TENGU_BRIDGE_AGENT", "bridged".to_string()),
        ("TENGU_BRIDGE_WORKSPACE", workspace.display().to_string()),
        (
            "TENGU_BRIDGE_TOOLS",
            json!([tool("read_file"), tool("list_directory")]).to_string(),
        ),
        ("TENGU_SECRETS_LOADED", "XM_E2E_BRIDGE_SECRET".to_string()),
        ("XM_E2E_BRIDGE_SECRET", SECRET.to_string()),
    ]);
    bridge.rpc(1, "initialize", json!({}));

    let read = bridge.call(2, "read_file", json!({"path": "notes.txt"}));
    assert_eq!(read["isError"], false, "{read}");
    assert_eq!(read["content"][0]["text"], "token=[REDACTED]\n", "{read}");

    let listed = bridge.call(3, "list_directory", json!({"path": "."}));
    assert_eq!(
        listed["isError"], true,
        "the agent's scope (no fs roots) must deny: {listed}"
    );
}
