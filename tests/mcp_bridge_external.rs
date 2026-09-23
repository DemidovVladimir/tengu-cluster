//! Claude Code subagents reach `[[mcp_servers]]` through the tengu MCP bridge.
//!
//! Spawns the real `tengu mcp-bridge` with the env the Claude Code engine
//! writes into its `--mcp-config` (`TENGU_BRIDGE_TOOLS` naming a
//! `{server}__{tool}` entry + `TENGU_BRIDGE_MCP_SERVERS`), pointing at
//! `tests/fixtures/fake_mcp_server.sh`, then speaks MCP to it: the external
//! tool must be listed and callable through the bridge.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

#[test]
fn bridge_proxies_external_mcp_server_tools() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/fake_mcp_server.sh"
    );
    let workspace = tempfile::TempDir::new().unwrap();
    let tools = json!([{
        "name": "fake__echo",
        "description": "Echo test tool",
        "parameters": {"type": "object", "properties": {}}
    }]);
    let servers = json!([{
        "name": "fake",
        "transport": "stdio",
        "command": ["sh", fixture]
    }]);

    let mut child = Command::new(env!("CARGO_BIN_EXE_tengu"))
        .arg("mcp-bridge")
        .env("TENGU_BRIDGE_WORKSPACE", workspace.path())
        .env("TENGU_BRIDGE_TOOLS", tools.to_string())
        .env("TENGU_BRIDGE_MCP_SERVERS", servers.to_string())
        .env("TENGU_EGRESS", r#"{"network":"open","audit":false}"#)
        .env("TENGU_CONFIG", workspace.path().join("absent.toml"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tengu mcp-bridge");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut rpc = |id: u64, method: &str, params: Value| -> Value {
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(stdin, "{req}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad reply {line:?}: {e}"))
    };

    rpc(1, "initialize", json!({}));
    let listed = rpc(2, "tools/list", json!({}));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(names, ["fake__echo"], "tools/list: {listed}");

    let called = rpc(
        3,
        "tools/call",
        json!({"name": "fake__echo", "arguments": {}}),
    );
    assert_eq!(
        called["result"]["content"][0]["text"], "pong",
        "tools/call: {called}"
    );

    child.kill().ok();
    child.wait().ok();
}
