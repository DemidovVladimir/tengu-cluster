//! Bridge conformance (`x-bridge-conformance-test`, tracker convention 20):
//! every catalog tool runs once in-process (`tengu tool call`: the executor a
//! `run-agent` child or a decision loop builds) and once through a real
//! `tengu mcp-bridge` subprocess (what a `claude_code` agent calls), on two
//! separate but identical fixture sandboxes. Both must agree.
//!
//! | Check | Test |
//! |---|---|
//! | Every `tengu tool list` name has a case (a catalog row cannot land without one); every case names a listed tool | `every_catalog_tool_has_a_case` |
//! | Per step: same `is_error`, same normalised text; the case's expected outcome | `bridge_matches_in_process` |
//! | Per case: same files left under the workspace and `TENGU_HOME` — SQLite stores as sorted table rows (`observations` without `observed_at_ms`), text as content | same |
//! | Per case: same upstream requests to the mock (sorted, normalised) | same |
//!
//! | Side | Process (cwd = its workspace) | Env (after `env_clear`) |
//! |---|---|---|
//! | in-process | one `tengu tool call -c <root>/config.toml --agent conf --batch`: a `{"tool", "args", "call_id": <n>}` line per step, one executor | `PATH`, `TMPDIR`, `HOME` + `TENGU_HOME` under the side root, `TENGU_SECRETS_LOADED` naming `XM_CONFORMANCE_SECRET`, the case env |
//! | bridge | one `tengu mcp-bridge`: `initialize`, then a `tools/call` per step with JSON-RPC id `<n>` | the same + `TENGU_CONFIG`, `TENGU_BRIDGE_AGENT=conf`, `TENGU_BRIDGE_WORKSPACE`, `TENGU_BRIDGE_TOOLS` (the agent's tools), `TENGU_AGENT_IPC=1` (a `run-agent` child's bridge: workspace root granted like in-process), `TENGU_BRIDGE_MCP_SERVERS` in `[[mcp_servers]]` cases |
//!
//! Fixture sandbox (`BASE_TOML` + the case's TOML): `[egress] network =
//! "open"`, `allow_hosts = ["127.0.0.1"]` (a hard-coded upstream host is
//! refused, never reached: no network), `audit = false`; `[agents.conf]` with
//! the side's workspace and the case's tools. Upstreams are a loopback
//! `Mock`: a route matches method + path + substrings of target and body and
//! answers inline JSON, a fixture file (`tests/fixtures/…`) or a
//! `getMultipleAccounts` reply built per request from captured accounts
//! (`Reply::Gma`). Tools with a base-URL override reach it through
//! `.scoped(<env>)` (`SOLANA_RPC_URL`, `HL_API_URL`: `POST /info` routes
//! by body `type`, captured replies in `tests/fixtures/hyperliquid/`).
//!
//! `normalize`, applied to both sides alike:
//!
//! | Volatile part | Becomes |
//! |---|---|
//! | the side's temp root (canonical and as given) | `<ROOT>` |
//! | durations `12ms` / `12 ms` | `<N>ms` |
//! | observation age after its status (`\| ok 3s`; line-start `ok 3s`) | `<AGE>s` |
//! | `*age_s` / `*age_ms` / `*age_secs` values (ages at read time; `max_*` / `min_*` limits kept) | `<AGE>` |
//! | `next_*_s` values (countdowns at read time: `next_funding_s`) | `<COUNTDOWN>` |
//! | ISO-8601 timestamps | `<TIME>` |
//! | history day files `YYYYMMDD.db` | `<DAY>.db` |
//! | epoch ms / s within 2 days of now (fixture timestamps stay) | `<EPOCH_MS>` / `<EPOCH_S>` |
//! | JSON-RPC `"id":<n>` (request bodies) | `"id":<N>` |
//!
//! Add a case: one `case("<tool>", json!({..}))` row in `cases()` plus the
//! TOML its scope needs, `.route(..)` replies and `.ok("…")` / `.err("…")`
//! (`docs/mcp-bridge.md` § Testing). `TENGU_CONFORMANCE_VERBOSE=1` (with
//! `--nocapture`) prints every case's in-process text, stores and requests.

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_tengu");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
/// Per process: a hang is a failure, not a stuck run.
const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);
/// Cases run on this many threads.
const WORKERS: usize = 8;
/// Registered for redaction on both sides (`TENGU_SECRETS_LOADED`).
const SECRET_VAR: &str = "XM_CONFORMANCE_SECRET";
const SECRET: &str = "sk-conformance-0123456789abcdef";
/// The fixture agent.
const AGENT: &str = "conf";

// Fixture ids (`tests/fixtures/solana/*/meta.json`), in full.
const WALLET: &str = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S";
/// Owner of three `POOL` positions (`solana/dlmm/golden.json`).
const LP_OWNER: &str = "JBggt27MzM4eohjumT9Tuec7MBoWAgDM4BJjkoisDUcs";
const POOL: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";
const POSITION: &str = "H9fmcxgheDvVSn9iUeRSvZPAgTY5WXqvroNpkZ2HCVRW";
const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const TX_OK: &str =
    "L8TEY2sSvscX2R2EBChD1p1o3HApdBUHqVfTJKLxJ4zK4M6pebL3diuYKcKjPF5deW7GF6DVnMdPU2tBwgz1Mfi";
const TOKENKEG: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

const BASE_TOML: &str = r#"
[egress]
network = "open"
allow_hosts = ["127.0.0.1"]
audit = false

[agents.main]
default = true
engine = "openrouter"
model = "anthropic/claude-haiku-4.5"

[agents.conf]
engine = "openrouter"
model = "anthropic/claude-haiku-4.5"
workspace = "{ws}"
tools = [{tools}]
"#;

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/// Expected outcome of a step: error or not, and a substring of the
/// normalised text ("" = any).
#[derive(Clone, Debug)]
struct Expect {
    error: bool,
    contains: String,
}

#[derive(Clone, Debug)]
struct Step {
    tool: String,
    args: Value,
    expect: Expect,
}

#[derive(Clone)]
struct Case {
    /// Unique label: `<tool>` or `<tool>:<variant>`.
    name: String,
    /// The catalog name this case covers.
    tool: String,
    steps: Vec<Step>,
    /// Appended to `BASE_TOML`; `{ws}`, `{root}`, `{mock}`, `{fixtures}` expanded.
    toml: String,
    /// `[agents.conf] tools`; default = every step's tool.
    agent_tools: Vec<String>,
    /// `.scoped(env)`: `[default_scopes.<t>]` for every agent tool (workspace
    /// store, mock host, `env` readable) and `env` = the mock.
    scope_env: Option<String>,
    env: Vec<(String, String)>,
    /// Workspace files; `{secret}` expanded.
    files: Vec<(String, String)>,
    routes: Vec<Route>,
    /// `[[mcp_servers]]` JSON handed to the bridge (`TENGU_BRIDGE_MCP_SERVERS`).
    mcp_servers: Option<Value>,
    /// The tool may be absent from this build (cargo feature): skipped then.
    gated: bool,
    /// Not a catalog tool (an `[[mcp_servers]]` proxy tool).
    extra: bool,
}

fn step(tool: &str, args: Value) -> Step {
    Step {
        tool: tool.to_string(),
        args,
        expect: Expect {
            error: false,
            contains: String::new(),
        },
    }
}

fn case(tool: &str, args: Value) -> Case {
    Case {
        name: tool.to_string(),
        tool: tool.to_string(),
        steps: vec![step(tool, args)],
        toml: String::new(),
        agent_tools: Vec::new(),
        scope_env: None,
        env: Vec::new(),
        files: Vec::new(),
        routes: Vec::new(),
        mcp_servers: None,
        gated: false,
        extra: false,
    }
}

impl Case {
    fn named(mut self, variant: &str) -> Self {
        self.name = format!("{}:{variant}", self.tool);
        self
    }
    fn last(&mut self) -> &mut Step {
        self.steps.last_mut().expect("a case has steps")
    }
    /// The last step succeeds with `s` in its text.
    fn ok(mut self, s: &str) -> Self {
        self.last().expect = Expect {
            error: false,
            contains: s.to_string(),
        };
        self
    }
    /// The last step fails with `s` in its text.
    fn err(mut self, s: &str) -> Self {
        self.last().expect = Expect {
            error: true,
            contains: s.to_string(),
        };
        self
    }
    /// Another call after the others (same workspace, same bridge process).
    fn then(mut self, tool: &str, args: Value) -> Self {
        self.steps.push(step(tool, args));
        self
    }
    /// A call before the others (e.g. the row a decide tool reads).
    fn before(mut self, tool: &str, args: Value) -> Self {
        self.steps.insert(0, step(tool, args));
        self
    }
    fn toml(mut self, s: &str) -> Self {
        self.toml.push('\n');
        self.toml.push_str(s);
        self
    }
    fn tools(mut self, tools: &[&str]) -> Self {
        self.agent_tools = tools.iter().map(|t| t.to_string()).collect();
        self
    }
    fn scoped(mut self, env: &str) -> Self {
        self.scope_env = Some(env.to_string());
        self
    }
    /// Solana tools: store, mock RPC host, `SOLANA_RPC_URL` → the mock
    /// (`docs/typed-observations-2026-09-24.md`).
    fn solana(self) -> Self {
        self.scoped("SOLANA_RPC_URL")
    }
    fn env(mut self, k: &str, v: &str) -> Self {
        self.env.push((k.to_string(), v.to_string()));
        self
    }
    fn file(mut self, path: &str, content: &str) -> Self {
        self.files.push((path.to_string(), content.to_string()));
        self
    }
    fn route(mut self, r: Route) -> Self {
        self.routes.push(r);
        self
    }
    fn gated(mut self) -> Self {
        self.gated = true;
        self
    }
    fn agent_tools(&self) -> Vec<String> {
        if !self.agent_tools.is_empty() {
            return self.agent_tools.clone();
        }
        let mut out: Vec<String> = Vec::new();
        for s in &self.steps {
            if !out.contains(&s.tool) {
                out.push(s.tool.clone());
            }
        }
        out
    }
    /// The case's TOML with the `.scoped` blocks.
    fn full_toml(&self) -> String {
        let mut toml = self.toml.clone();
        if let Some(env) = &self.scope_env {
            for t in self.agent_tools() {
                toml.push_str(&format!(
                    "\n[default_scopes.{t}]\nfs_roots = [\"{{ws}}\"]\nnet_hosts = [\"127.0.0.1\"]\nenv_reads = [\"{env}\"]\n"
                ));
            }
        }
        toml
    }
    fn full_env(&self) -> Vec<(String, String)> {
        let mut env = self.env.clone();
        if let Some(var) = &self.scope_env {
            env.push((var.clone(), "{mock}".to_string()));
        }
        env
    }
}

/// `hedge_decide` knobs (all required).
fn hedge_knobs() -> Value {
    json!({
        "target_delta_sol": 0.0, "delta_threshold_sol": 0.5, "band_bins": 0, "bin_count": 20,
        "cap_mult": 1.0, "max_notional_usd": 0.0, "min_collateral_ratio": 0.2,
        "target_collateral_ratio": 0.3, "carry_cap_bps": 0.0, "cooldown_ms": 0,
        "lp_input": "live", "include_wallet_sol": false, "min_wallet_sol": 0.1,
        "rent_reserve_sol": 0.05, "max_divergence_bps": 100.0, "max_snapshot_age_secs": 30,
        "trend_confirm_ms": 0, "no_lp_grace_ms": 0
    })
}

/// `lp_decide` knobs (all required).
fn lp_knobs() -> Value {
    json!({
        "imbalance_threshold": 0.8, "bin_count": 20, "storm_pct_5m": 0.0,
        "trend_confirm_ms": 0, "reentry_confirm_ms": 0, "reentry_tol_frac": 0.2,
        "max_divergence_bps": 100.0, "max_snapshot_age_secs": 30,
        "min_wallet_sol": 0.1, "rent_reserve_sol": 0.05
    })
}

/// A `getProgramAccounts` reply listing `keys` (discovery reads `pubkey` only).
fn gpa(keys: &[&str]) -> String {
    let rows: Vec<Value> = keys.iter().map(|k| json!({"pubkey": k})).collect();
    json!({"jsonrpc": "2.0", "id": 1,
           "result": {"context": {"slot": 450102095}, "value": rows}})
    .to_string()
}

/// The wallet's token accounts under both programs (`solana/wallet`).
fn token_accounts(c: Case) -> Case {
    c.route(
        rpc("getTokenAccountsByOwner")
            .has(TOKENKEG)
            .file("solana/wallet/token_accounts_tokenkeg.json"),
    )
    .route(
        rpc("getTokenAccountsByOwner")
            .has(TOKEN_2022)
            .file("solana/wallet/token_accounts_token2022.json"),
    )
}

/// The `lp_snapshot` a decide tool reads: the wallet's pool, perps and
/// accounts from the captures, no positions, no oracle (Jupiter refused).
fn snapshot(c: Case) -> Case {
    c.before("lp_snapshot", json!({"wallet": WALLET, "pool": POOL}))
        .route(rpc("getProgramAccounts").json(&gpa(&[])))
        .route(rpc("getMultipleAccounts").gma(&[DLMM_GMA, PERPS_GMA, WALLET_GMA]))
}

/// Every reply `hl_ctx` needs for the HIP-3 dex `xyz`: its ctx + at-cap
/// reads and the perp meta (`hyperliquid/meta.json` captures).
fn hl_xyz(c: Case) -> Case {
    c.route(
        info("metaAndAssetCtxs")
            .has("\"dex\":\"xyz\"")
            .file("hyperliquid/metaAndAssetCtxs_xyz.json"),
    )
    .route(
        info("perpsAtOpenInterestCap")
            .has("\"dex\":\"xyz\"")
            .file("hyperliquid/perpsAtOpenInterestCap_xyz.json"),
    )
    .route(info("perpDexs").file("hyperliquid/perpDexs.json"))
    .route(info("perpCategories").file("hyperliquid/perpCategories.json"))
}

/// A skill in the project tier (`<cwd>/skills/demo`).
const DEMO_SKILL: &str = "---\nname: demo\ndescription: Conformance demo skill.\neditable_by_learner: true\n---\n\n# demo\n\nBody.\n";

/// One case per catalog tool (a few with variants), plus extras.
fn cases() -> Vec<Case> {
    let fake_server = format!("{FIXTURES}/fake_mcp_server.sh");
    let mut cases = vec![
        // ── workspace ──────────────────────────────────────────────────
        case("read_file", json!({"path": "notes.txt"}))
            .file("notes.txt", "token={secret}\nline two\n")
            .ok("token=[REDACTED]"),
        // A tool outside the agent's `tools` allow-list is refused on both.
        case("read_file", json!({"path": "notes.txt"}))
            .named("not_allowed")
            .tools(&["list_directory"])
            .file("notes.txt", "x")
            .err("not available to this agent"),
        case("list_directory", json!({"path": "."}))
            .file("a.txt", "a")
            .file("sub/b.txt", "b")
            .ok("sub/"),
        case("write_file", json!({"path": "out/new.txt", "content": "written"}))
            .then("read_file", json!({"path": "out/new.txt"}))
            .ok("written"),
        case("run_command", json!({"command": "echo conformance"})).ok("conformance"),
        case("run_command", json!({"command": "echo denied"}))
            .named("agent_scope")
            .toml("[agents.conf.scopes.run_command]\nshell_bins = [\"ls\"]\n")
            .err("not in allowed shell_bins"),
        // A signer makes every agent shell-free (`no_shell_fallback`).
        case("run_command", json!({"command": "echo denied"}))
            .named("no_shell")
            .toml("[solana]\nsigner_key_file = \"{root}/keys/signer.json\"\n")
            .err("not in allowed shell_bins"),
        // ── memory (no embeddings API in tests) ────────────────────────
        case("memory_ingest", json!({"content": "a fact"}))
            .toml("[memory]\nenabled = true\n")
            .err("no vector backend"),
        case("memory_search", json!({"query": "a fact"}))
            .toml("[memory]\nenabled = true\n")
            .err("no vector backend"),
        case("persistent_store", json!({"operation": "list"}))
            .toml("[memory]\nenabled = true\n")
            .ok("\"count\":0"),
        // ── cache ──────────────────────────────────────────────────────
        case(
            "shared_cache",
            json!({"operation": "put", "namespace": "ns", "key": "k", "value": "\"v\""}),
        )
        .then(
            "shared_cache",
            json!({"operation": "get", "namespace": "ns", "key": "k"}),
        )
        .ok("\"v\""),
        // ── skills (project tier = <cwd>/skills) ───────────────────────
        case("view_skill", json!({"action": "read", "skill": "demo"}))
            .file("skills/demo/SKILL.md", DEMO_SKILL)
            .ok("Conformance demo skill."),
        case(
            "skill_resource",
            json!({"action": "read", "skill": "demo", "path": "notes.md"}),
        )
        .file("skills/demo/SKILL.md", DEMO_SKILL)
        .file("skills/demo/resources/notes.md", "resource body\n")
        .ok("resource body"),
        case(
            "manage_skill",
            json!({"action": "create", "name": "made-here", "tier": "workspace",
                   "description": "Made by the conformance test.", "body": "# made-here\n\nBody.\n"}),
        )
        .then("view_skill", json!({"action": "read", "skill": "made-here"}))
        .ok("Made by the conformance test."),
        case(
            "skill_distill",
            json!({"name": "distilled", "description": "Distilled by the conformance test.",
                   "body_markdown": "# distilled\n\nBody.\n", "metrics": [],
                   "from_message_index": 0, "tier": "workspace"}),
        )
        .then("view_skill", json!({"action": "read", "skill": "distilled"}))
        .ok("Distilled by the conformance test."),
        case(
            "apply_improver_proposal",
            json!({"skill": "demo", "body_markdown": "# demo\n\nImproved body.\n",
                   "rationale": "conformance"}),
        )
        .file("skills/demo/SKILL.md", DEMO_SKILL)
        .then("view_skill", json!({"action": "read", "skill": "demo"}))
        .ok("Improved body."),
        // ── http ───────────────────────────────────────────────────────
        case(
            "http_request",
            json!({"url": "{mock}/echo", "method": "GET", "return_body": true}),
        )
        .toml("[default_scopes.http_request]\nnet_hosts = [\"127.0.0.1\"]\n")
        .route(get("/echo").json(&format!("{{\"ok\":true,\"token\":\"{SECRET}\"}}")))
        .ok("\"token\":\"[REDACTED]\""),
        // `$VAR` in the URL expands only where the scope's env_reads allows.
        case(
            "http_request",
            json!({"url": "$XM_CONFORMANCE_API/submit", "method": "POST",
                   "body": "{\"q\":1}", "return_body": true}),
        )
        .named("post_env_url")
        .toml("[default_scopes.http_request]\nnet_hosts = [\"127.0.0.1\"]\nenv_reads = [\"XM_CONFORMANCE_API\"]\n")
        .env("XM_CONFORMANCE_API", "{mock}")
        .route(post("/submit").has("\"q\":1").json("{\"accepted\":true}"))
        .ok("\"accepted\":true"),
        case(
            "http_request",
            json!({"url": "https://example.com/", "method": "GET"}),
        )
        .named("egress_denied")
        .err("not in allow_hosts"),
        // ── crypto (Privy is not configured in tests) ──────────────────
        case("sign_and_send_transaction", json!({"to": WALLET, "value": "1"}))
            .err("PRIVY_APP_ID"),
        case("sign_message", json!({"message": "hello"})).err("PRIVY_APP_ID"),
        case("get_wallet_address", json!({})).err("PRIVY_APP_ID"),
        case(
            "abi_encode",
            json!({"function_signature": "transfer(address,uint256)",
                   "args": ["0x000000000000000000000000000000000000dEaD", "1"]}),
        )
        .ok("0xa9059cbb"),
        case("hex_to_uint256", json!({"hex": "0xff"})).ok("255"),
        // ── agentic_memory (feature postgres_memory; no database) ───────
        case(
            "agentic_memory",
            json!({"operation": "recall", "query": "anything"}),
        )
        .gated()
        .err("TENGU_MEMORY_DATABASE_URL"),
        // ── Solana reads: RPC → the mock; Jupiter / Meteora datapi hosts
        //    are hard-coded, so egress refuses them (an error field / row)
        case("sol_price", json!({"mint": WSOL}))
            .solana()
            .ok("not in allow_hosts"),
        case("dlmm_pools", json!({"query": "SOL-USDC"}))
            .solana()
            .ok("not in allow_hosts"),
        case("dlmm_pool", json!({"pool": POOL}))
            .solana()
            .route(rpc("getMultipleAccounts").gma(&[DLMM_GMA]))
            .ok("active_id=-5373"),
        case("dlmm_positions", json!({"wallet": LP_OWNER, "pool": POOL}))
            .solana()
            .route(rpc("getProgramAccounts").json(&gpa(&[
                "Ui6V49xptXnsgay1h75MVch6zh2B3ahP2yb7iRTwX6x",
                "vpyRvdCUxaWRqRnguXzzqHyvg3Hejq7zXwBzceUEwdq",
                POSITION,
            ])))
            .route(rpc("getMultipleAccounts").gma(&[DLMM_GMA]))
            .ok("discovery=found"),
        case("jup_perps", json!({"wallet": WALLET}))
            .solana()
            .route(rpc("getMultipleAccounts").gma(&[PERPS_GMA]))
            .ok("long=flat short=flat"),
        // `[xmarket]` + `[recorder]` reach the bridge: history day files.
        case("jup_perps", json!({"wallet": WALLET}))
            .named("recorder")
            .solana()
            .toml("[xmarket]\nstate = \"conf\"\n\n[recorder]\nenabled = true\nschemas = [\"*\"]\n")
            .route(rpc("getMultipleAccounts").gma(&[PERPS_GMA]))
            .ok("long=flat short=flat"),
        token_accounts(
            case("solana_wallet", json!({"wallet": WALLET}))
                .solana()
                .route(rpc("getBalance").file("solana/wallet/get_balance.json"))
                .route(rpc("getMultipleAccounts").gma(&[WALLET_GMA])),
        )
        .ok("USDC=107.808931"),
        case("solana_tx", json!({"signature": TX_OK}))
            .solana()
            .route(rpc("getSignatureStatuses").file("solana/wallet/sig_statuses_history.json"))
            .route(rpc("getTransaction").file("solana/wallet/tx_ok.json"))
            .ok("finalized ok"),
        case("lp_snapshot", json!({"wallet": WALLET, "pool": POOL}))
            .solana()
            .route(rpc("getProgramAccounts").json(&gpa(&[])))
            .route(rpc("getMultipleAccounts").gma(&[DLMM_GMA, PERPS_GMA, WALLET_GMA]))
            .ok("perps=flat"),
        snapshot(case(
            "hedge_decide",
            json!({"wallet": WALLET, "pool": POOL, "knobs": hedge_knobs()}),
        ))
        .solana()
        .ok("action="),
        snapshot(case(
            "lp_decide",
            json!({"wallet": WALLET, "pool": POOL, "knobs": lp_knobs()}),
        ))
        .solana()
        .ok("verdict="),
        // ── Solana writes: simulate through the mock; send refused ──────
        token_accounts(case(
            "solana_close_token_accounts",
            json!({"wallet": WALLET}),
        ))
        .solana()
        .ok("simulate noop"),
        case(
            "jupiter_swap",
            json!({"wallet": WALLET, "input_mint": WSOL, "output_mint": USDC,
                   "amount": 0.001, "oracle_gate_bps": 50, "mode": "send"}),
        )
        .solana()
        .ok("signer_not_allowed"),
        // Jupiter Ultra is a hard-coded host: the simulate read is refused.
        case(
            "jupiter_swap",
            json!({"wallet": WALLET, "input_mint": WSOL, "output_mint": USDC,
                   "amount": 0.001, "oracle_gate_bps": 50}),
        )
        .named("simulate")
        .solana()
        .ok("simulate refused"),
        case(
            "dlmm_close_position",
            json!({"wallet": LP_OWNER, "pool": POOL, "position": POSITION,
                   "arm_reentry": false, "mode": "send"}),
        )
        .solana()
        .ok("signer_not_allowed"),
        case(
            "dlmm_open_position",
            json!({"wallet": WALLET, "pool": POOL, "amount_x": 0.01, "amount_y": 0,
                   "bin_count": 20, "strategy": "spot", "max_active_bin_slippage": 1,
                   "min_wallet_sol": 0.1, "max_new_bin_arrays": 0, "max_divergence_bps": 50,
                   "mode": "send"}),
        )
        .solana()
        .ok("signer_not_allowed"),
        case(
            "jup_perps_order",
            json!({"wallet": WALLET, "pool": POOL, "side": "short", "action": "increase",
                   "size_usd": 10, "collateral": 5, "slippage_bps": 50,
                   "max_notional_usd": 100, "mode": "send"}),
        )
        .solana()
        .ok("signer_not_allowed"),
        // The agent's own scope (wallet granted) reaches the tool through
        // the bridge: the refusal moves on to the missing signer.
        case(
            "jup_perps_order",
            json!({"wallet": WALLET, "pool": POOL, "side": "short", "action": "close",
                   "slippage_bps": 50, "max_notional_usd": 100, "mode": "send"}),
        )
        .named("wallet_granted")
        .toml(&format!(
            "[agents.conf.scopes.jup_perps_order]\nfs_roots = [\"{{ws}}\"]\nnet_hosts = [\"127.0.0.1\"]\nenv_reads = [\"SOLANA_RPC_URL\"]\nwallets = [\"{WALLET}\"]\n"
        ))
        .env("SOLANA_RPC_URL", "{mock}")
        .ok("no_signer"),
        // ── Hyperliquid reads: `HL_API_URL` → the mock ─────────────────
        // One coin: the whole xyz dex is written (2 × 128 rows + meta + cap).
        hl_xyz(case("hl_ctx", json!({"coins": ["xyz:TSLA"]})))
            .scoped("HL_API_URL")
            .ok("mkt hyperliquid:xyz:TSLA mark=347.19"),
    ];
    // ── [[mcp_servers]] proxy tool (not a catalog row) ─────────────────
    let mut proxy = case("fake__echo", json!({}))
        .toml(&format!(
            "[[mcp_servers]]\nname = \"fake\"\ntransport = \"stdio\"\ncommand = [\"sh\", \"{fake_server}\"]\n"
        ))
        .ok("pong");
    proxy.extra = true;
    proxy.mcp_servers = Some(json!([{
        "name": "fake", "transport": "stdio", "command": ["sh", fake_server]
    }]));
    cases.push(proxy);
    cases
}

/// A captured `getMultipleAccounts`: (meta file, JSON pointer to its key
/// list — strings or `{"pubkey": ..}` — , gma file).
type GmaSource = (&'static str, &'static str, &'static str);
const DLMM_GMA: GmaSource = ("solana/dlmm/meta.json", "/keys", "solana/dlmm/gma.json");
const PERPS_GMA: GmaSource = (
    "solana/perps/meta.json",
    "/gma/keys",
    "solana/perps/gma.json",
);
const WALLET_GMA: GmaSource = (
    "solana/wallet/meta.json",
    "/files/gma_base64.json/keys",
    "solana/wallet/gma_base64.json",
);

// ---------------------------------------------------------------------------
// Mock upstream
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Reply {
    /// Inline body.
    Json(String),
    /// A file under `tests/fixtures/`.
    File(&'static str),
    /// `getMultipleAccounts` from captured accounts, in the requested order
    /// (`null` for an unknown key), at the first source's slot.
    Gma(Vec<GmaSource>),
}

#[derive(Clone)]
struct Route {
    method: &'static str,
    path: &'static str,
    /// Substrings of target + body, all required.
    has: Vec<String>,
    reply: Reply,
}

fn route(method: &'static str, path: &'static str) -> Route {
    Route {
        method,
        path,
        has: Vec::new(),
        reply: Reply::Json("{}".into()),
    }
}

fn get(path: &'static str) -> Route {
    route("GET", path)
}

fn post(path: &'static str) -> Route {
    route("POST", path)
}

/// A Solana JSON-RPC method on the mock root.
fn rpc(method: &str) -> Route {
    post("/").has(&format!("\"method\":\"{method}\""))
}

/// A Hyperliquid `POST /info` request of `type` (`HL_API_URL` = the mock).
fn info(t: &str) -> Route {
    post("/info").has(&format!("\"type\":\"{t}\""))
}

impl Route {
    fn has(mut self, s: &str) -> Self {
        self.has.push(s.to_string());
        self
    }
    fn json(mut self, body: &str) -> Self {
        self.reply = Reply::Json(body.to_string());
        self
    }
    fn file(mut self, path: &'static str) -> Self {
        self.reply = Reply::File(path);
        self
    }
    fn gma(mut self, sources: &[GmaSource]) -> Self {
        self.reply = Reply::Gma(sources.to_vec());
        self
    }
}

/// Loopback HTTP/1.1 server: one request per connection, the first
/// matching route (else 404), every request logged.
struct Mock {
    base: String,
    log: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    fn start(routes: Vec<Route>) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let routes = Arc::new(routes);
        let thread_log = Arc::clone(&log);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let routes = Arc::clone(&routes);
                let log = Arc::clone(&thread_log);
                std::thread::spawn(move || serve(stream, &routes, &log));
            }
        });
        Mock { base, log }
    }

    fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

fn serve(stream: TcpStream, routes: &[Route], log: &Mutex<Vec<String>>) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok();
    let body = String::from_utf8_lossy(&body).to_string();
    log.lock()
        .unwrap()
        .push(format!("{method} {target} {body}").trim_end().to_string());

    let path = target.split('?').next().unwrap_or("");
    let haystack = format!("{target}\n{body}");
    let matched = routes.iter().find(|r| {
        r.method == method && r.path == path && r.has.iter().all(|h| haystack.contains(h.as_str()))
    });
    let (status, reply) = match matched {
        Some(r) => ("200 OK", render_reply(&r.reply, &body)),
        None => (
            "404 Not Found",
            json!({"error": format!("no mock route for {method} {target}")}).to_string(),
        ),
    };
    let mut out = stream;
    let _ = write!(
        out,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
    let _ = out.flush();
}

fn fixture(path: &str) -> String {
    std::fs::read_to_string(Path::new(FIXTURES).join(path))
        .unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

fn render_reply(reply: &Reply, request_body: &str) -> String {
    match reply {
        Reply::Json(s) => s.clone(),
        Reply::File(p) => fixture(p),
        Reply::Gma(sources) => {
            let mut table: HashMap<String, Value> = HashMap::new();
            let mut slot = None;
            for (meta, pointer, gma) in sources {
                let meta: Value = serde_json::from_str(&fixture(meta)).unwrap();
                let gma: Value = serde_json::from_str(&fixture(gma)).unwrap();
                slot.get_or_insert(gma["result"]["context"]["slot"].clone());
                let keys = meta.pointer(pointer).and_then(Value::as_array).unwrap();
                let values = gma["result"]["value"].as_array().unwrap();
                for (k, v) in keys.iter().zip(values) {
                    let k = k.as_str().or_else(|| k["pubkey"].as_str()).unwrap();
                    table.entry(k.to_string()).or_insert_with(|| v.clone());
                }
            }
            let req: Value = serde_json::from_str(request_body).unwrap_or(Value::Null);
            let value: Vec<Value> = req["params"][0]
                .as_array()
                .map(|keys| {
                    keys.iter()
                        .map(|k| {
                            k.as_str()
                                .and_then(|k| table.get(k).cloned())
                                .unwrap_or(Value::Null)
                        })
                        .collect()
                })
                .unwrap_or_default();
            json!({"jsonrpc": "2.0", "id": req["id"], "result": {
                "context": {"slot": slot.unwrap_or(Value::Null)}, "value": value}})
            .to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// One side: a sandbox root and its processes
// ---------------------------------------------------------------------------

struct Side {
    _dir: tempfile::TempDir,
    /// Every spelling of the root (canonical first), for `normalize`.
    roots: Vec<String>,
    root: PathBuf,
    ws: PathBuf,
    config: PathBuf,
}

impl Side {
    fn new(case: &Case, mock: &str) -> Side {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(root.join("home/.tengu")).unwrap();
        for (path, content) in &case.files {
            let p = ws.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content.replace("{secret}", SECRET)).unwrap();
        }
        let tools: Vec<String> = case
            .agent_tools()
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect();
        let toml = format!("{BASE_TOML}{}", case.full_toml()).replace("{tools}", &tools.join(", "));
        let config = root.join("config.toml");
        std::fs::write(&config, expand(&toml, &root, &ws, mock)).unwrap();
        let mut roots = vec![root.display().to_string()];
        let given = dir.path().display().to_string();
        if given != roots[0] {
            roots.push(given);
        }
        Side {
            _dir: dir,
            roots,
            root,
            ws,
            config,
        }
    }

    /// `env_clear` + what both processes get.
    fn command(&self, case: &Case, mock: &str) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(&self.ws)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("TENGU_HOME", self.root.join("home/.tengu"))
            .env("TENGU_SECRETS_LOADED", SECRET_VAR)
            .env(SECRET_VAR, SECRET)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Ok(tmp) = std::env::var("TMPDIR") {
            cmd.env("TMPDIR", tmp);
        }
        for (k, v) in case.full_env() {
            cmd.env(k, expand(&v, &self.root, &self.ws, mock));
        }
        cmd
    }

    fn args(&self, step: &Step, mock: &str) -> Value {
        serde_json::from_str(&expand(&step.args.to_string(), &self.root, &self.ws, mock))
            .expect("args stay JSON")
    }
}

fn expand(s: &str, root: &Path, ws: &Path, mock: &str) -> String {
    s.replace("{ws}", &ws.display().to_string())
        .replace("{root}", &root.display().to_string())
        .replace("{mock}", mock)
        .replace("{fixtures}", FIXTURES)
}

/// One side's answer to one step.
#[derive(Debug)]
struct Answer {
    is_error: bool,
    text: String,
}

/// Drain the pipes still attached on threads; kill after `PROCESS_TIMEOUT`.
fn wait_output(mut child: Child) -> Result<(String, String), String> {
    fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut p) = pipe {
                p.read_to_string(&mut s).ok();
            }
            s
        })
    }
    let o = drain(child.stdout.take());
    let e = drain(child.stderr.take());
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        if Instant::now() > deadline {
            child.kill().ok();
            child.wait().ok();
            return Err(format!(
                "timed out after {PROCESS_TIMEOUT:?}; stderr: {}",
                tail(&e.join().unwrap_or_default())
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok((o.join().unwrap_or_default(), e.join().unwrap_or_default()))
}

fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(8)..].join("\n")
}

/// Step `i`'s call id on both sides (the bridge's JSON-RPC id).
fn call_id(i: usize) -> u64 {
    i as u64 + 2
}

/// In-process: one `tengu tool call --batch` (one executor, like the
/// bridge's one session), a line per step.
fn run_in_process(side: &Side, case: &Case, mock: &str) -> Result<Vec<Answer>, String> {
    let mut cmd = side.command(case, mock);
    cmd.args(["tool", "call", "-c"])
        .arg(&side.config)
        .args(["--agent", AGENT, "--batch"]);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn tengu tool call: {e}"))?;
    {
        // A process that died early shows below, with its stderr.
        let mut stdin = child.stdin.take().unwrap();
        for (i, step) in case.steps.iter().enumerate() {
            let line =
                json!({"tool": step.tool, "args": side.args(step, mock), "call_id": call_id(i)});
            let _ = writeln!(stdin, "{line}");
        }
    }
    let (stdout, stderr) = wait_output(child)?;
    let answers: Vec<Answer> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|v| Answer {
            is_error: v["is_error"].as_bool().unwrap_or(true),
            text: v["text"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    if answers.len() != case.steps.len() {
        return Err(format!(
            "tengu tool call answered {} of {} steps: {stdout:?}; stderr: {}",
            answers.len(),
            case.steps.len(),
            tail(&stderr)
        ));
    }
    Ok(answers)
}

/// Bridge: one `tengu mcp-bridge`, `initialize`, a `tools/call` per step.
fn run_bridge(side: &Side, case: &Case, mock: &str) -> Result<Vec<Answer>, String> {
    let defs: Vec<Value> = case
        .agent_tools()
        .iter()
        .map(|n| json!({"name": n, "description": "d", "parameters": {"type": "object", "properties": {}}}))
        .collect();
    let mut cmd = side.command(case, mock);
    cmd.arg("mcp-bridge")
        .env("TENGU_CONFIG", &side.config)
        .env("TENGU_BRIDGE_AGENT", AGENT)
        .env("TENGU_BRIDGE_WORKSPACE", &side.ws)
        .env("TENGU_BRIDGE_TOOLS", Value::from(defs).to_string())
        .env("TENGU_AGENT_IPC", "1");
    if let Some(servers) = &case.mcp_servers {
        cmd.env("TENGU_BRIDGE_MCP_SERVERS", servers.to_string());
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn tengu mcp-bridge: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut rpc_call = |req: Value| -> Result<Value, String> {
        writeln!(stdin, "{req}").map_err(|e| format!("bridge stdin: {e}"))?;
        stdin.flush().ok();
        let line = rx
            .recv_timeout(PROCESS_TIMEOUT)
            .map_err(|e| format!("no bridge reply to {req}: {e}"))?;
        serde_json::from_str(&line).map_err(|e| format!("bad bridge reply {line:?}: {e}"))
    };
    let mut answers = Vec::new();
    let mut run = || -> Result<(), String> {
        rpc_call(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))?;
        for (i, step) in case.steps.iter().enumerate() {
            let r = rpc_call(
                json!({"jsonrpc": "2.0", "id": call_id(i), "method": "tools/call",
                                    "params": {"name": step.tool, "arguments": side.args(step, mock)}}),
            )?;
            let result = &r["result"];
            answers.push(Answer {
                is_error: result["isError"].as_bool().unwrap_or(true),
                text: result["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        Ok(())
    };
    let outcome = run();
    drop(rpc_call);
    drop(stdin);
    let (_, stderr) = wait_output(child)?;
    outcome.map_err(|e| format!("{e}; bridge stderr: {}", tail(&stderr)))?;
    Ok(answers)
}

// ---------------------------------------------------------------------------
// Normaliser + side effects
// ---------------------------------------------------------------------------

fn normalize(s: &str, roots: &[String]) -> String {
    use once_cell::sync::Lazy;
    use regex::Regex;
    static RULES: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
        [
            (r"\b\d+(\.\d+)? ?ms\b", "<N>ms"),
            (r"(?m)(\| [a-z_]+|^[a-z_]+) \d+s\b", "$1 <AGE>s"),
            (
                r"\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?",
                "<TIME>",
            ),
            (r"\b20\d{6}\.db\b", "<DAY>.db"),
            (r#""id":\d+"#, r#""id":<N>"#),
        ]
        .into_iter()
        .map(|(re, to)| (Regex::new(re).unwrap(), to))
        .collect()
    });
    // `<key>age_s|ms|secs` = number; `max_` / `min_` keys are limits, kept.
    static AGE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"\b(\w*?)(age_(?:s|ms|secs)\b"?\s?[=:]\s?)-?\d+(\.\d+)?"#).unwrap()
    });
    // `next_<event>_s` = seconds until the event at read time (not `_ms`
    // epoch stamps, which `EPOCH` covers).
    static COUNTDOWN: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"\b(next_\w*?_s\b"?\s?[=:]\s?)-?\d+(\.\d+)?"#).unwrap());
    static EPOCH: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b1\d{9}(\d{3})?\b").unwrap());
    const WINDOW_S: i64 = 2 * 86_400;
    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let mut out = s.to_string();
    for r in roots {
        out = out.replace(r.as_str(), "<ROOT>");
    }
    for (re, to) in RULES.iter() {
        out = re.replace_all(&out, *to).into_owned();
    }
    out = AGE
        .replace_all(&out, |c: &regex::Captures<'_>| {
            let key = &c[1];
            if key.starts_with("max_") || key.starts_with("min_") {
                c[0].to_string()
            } else {
                format!("{key}{}<AGE>", &c[2])
            }
        })
        .into_owned();
    out = COUNTDOWN.replace_all(&out, "${1}<COUNTDOWN>").into_owned();
    EPOCH
        .replace_all(&out, |c: &regex::Captures<'_>| {
            let digits = &c[0];
            let v: i64 = digits.parse().unwrap_or(0);
            match digits.len() {
                13 if (v / 1000 - now_s).abs() <= WINDOW_S => "<EPOCH_MS>".to_string(),
                10 if (v - now_s).abs() <= WINDOW_S => "<EPOCH_S>".to_string(),
                _ => digits.to_string(),
            }
        })
        .into_owned()
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// Every file left under the workspace and the side's home (`TENGU_HOME`
/// inside), normalised: SQLite databases as sorted table rows — in
/// `observations`, `observed_at_ms` dropped from columns and body — text
/// files as content, other files as a size. SQLite side files skipped.
fn side_effects(side: &Side) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    walk(&side.ws, &mut files);
    walk(&side.root.join("home"), &mut files);
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let rel = f.strip_prefix(&side.root).unwrap().display().to_string();
        if ["-wal", "-shm", "-journal"]
            .iter()
            .any(|s| rel.ends_with(s))
        {
            continue;
        }
        let bytes = std::fs::read(&f).map_err(|e| format!("read {rel}: {e}"))?;
        if bytes.starts_with(b"SQLite format 3\0") {
            out.extend(db_rows(&f, &rel)?);
            continue;
        }
        match String::from_utf8(bytes) {
            Ok(text) => out.push(format!("{rel}: {text}")),
            Err(e) => out.push(format!("{rel}: {} bytes", e.as_bytes().len())),
        }
    }
    Ok(out.iter().map(|l| normalize(l, &side.roots)).collect())
}

fn db_rows(path: &Path, rel: &str) -> Result<Vec<String>, String> {
    use rusqlite::types::ValueRef;
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("open {rel}: {e}"))?;
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .and_then(|mut s| s.query_map([], |r| r.get(0))?.collect())
        .map_err(|e| format!("{rel}: {e}"))?;
    let mut out = Vec::new();
    for table in tables {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM \"{table}\""))
            .map_err(|e| format!("{rel}/{table}: {e}"))?;
        let cols: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
        let mut rows = stmt.query([]).map_err(|e| format!("{rel}/{table}: {e}"))?;
        let mut lines = Vec::new();
        while let Some(row) = rows.next().map_err(|e| format!("{rel}/{table}: {e}"))? {
            let mut fields = Vec::new();
            for (i, col) in cols.iter().enumerate() {
                if table == "observations" && col == "observed_at_ms" {
                    continue;
                }
                let v = match row.get_ref(i).map_err(|e| e.to_string())? {
                    ValueRef::Null => "null".to_string(),
                    ValueRef::Integer(n) => n.to_string(),
                    ValueRef::Real(x) => x.to_string(),
                    ValueRef::Text(t) => {
                        let t = String::from_utf8_lossy(t).to_string();
                        match serde_json::from_str::<Value>(&t) {
                            Ok(Value::Object(mut o)) if table == "observations" => {
                                o.remove("observed_at_ms");
                                Value::Object(o).to_string()
                            }
                            _ => t,
                        }
                    }
                    ValueRef::Blob(b) => format!("<{} bytes>", b.len()),
                };
                fields.push(format!("{col}={v}"));
            }
            lines.push(format!("{rel}/{table}: {}", fields.join(" | ")));
        }
        lines.sort();
        out.push(format!("{rel}/{table}: {} rows", lines.len()));
        out.extend(lines);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Running a case
// ---------------------------------------------------------------------------

fn run_case(case: &Case) -> Result<(), String> {
    let mock = Mock::start(case.routes.clone());
    let local = Side::new(case, &mock.base);
    let bridged = Side::new(case, &mock.base);

    let a = run_in_process(&local, case, &mock.base)?;
    let split = mock.requests().len();
    let b = run_bridge(&bridged, case, &mock.base)?;
    let requests = mock.requests();
    let (fa, fb) = (side_effects(&local)?, side_effects(&bridged)?);
    let norm_requests = |reqs: &[String], side: &Side| -> Vec<String> {
        let mut v: Vec<String> = reqs.iter().map(|r| normalize(r, &side.roots)).collect();
        v.sort();
        v
    };
    let (qa, qb) = (
        norm_requests(&requests[..split], &local),
        norm_requests(&requests[split..], &bridged),
    );

    if std::env::var_os("TENGU_CONFORMANCE_VERBOSE").is_some() {
        let steps: Vec<String> = a
            .iter()
            .zip(&case.steps)
            .map(|(x, s)| {
                let t = normalize(&x.text, &local.roots);
                format!("  {} is_error={}: {t}", s.tool, x.is_error)
            })
            .collect();
        eprintln!(
            "── {} ──\n{}\n  files {fa:#?}\n  requests {qa:#?}",
            case.name,
            steps.join("\n")
        );
    }

    let mut problems = Vec::new();
    for (i, step) in case.steps.iter().enumerate() {
        let (x, y) = (&a[i], &b[i]);
        let tx = normalize(&x.text, &local.roots);
        let ty = normalize(&y.text, &bridged.roots);
        if x.is_error != y.is_error || tx != ty {
            problems.push(format!(
                "step {i} ({}): in-process is_error={}:\n{tx}\n--- bridge is_error={}:\n{ty}",
                step.tool, x.is_error, y.is_error
            ));
        }
        if x.is_error != step.expect.error || !tx.contains(&step.expect.contains) {
            problems.push(format!(
                "step {i} ({}): expected is_error={} with {:?}, got is_error={}:\n{tx}",
                step.tool, step.expect.error, step.expect.contains, x.is_error
            ));
        }
    }
    if fa != fb {
        problems.push(format!(
            "files / store rows differ:\nin-process {fa:#?}\nbridge {fb:#?}"
        ));
    }
    if qa != qb {
        problems.push(format!(
            "upstream requests differ:\nin-process {qa:#?}\nbridge {qb:#?}"
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

/// `tengu tool list`: this build's catalog.
fn catalog() -> Vec<String> {
    let out = Command::new(BIN)
        .args(["tool", "list"])
        .env_remove("TENGU_CONFIG")
        .output()
        .expect("run tengu tool list");
    assert!(out.status.success(), "tengu tool list failed: {out:?}");
    serde_json::from_slice(&out.stdout).expect("tool list prints a JSON array")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Convention 20: a catalog row without a conformance case fails CI.
#[test]
fn every_catalog_tool_has_a_case() {
    let catalog = catalog();
    let cases = cases();
    let covered: BTreeSet<&str> = cases.iter().map(|c| c.tool.as_str()).collect();
    let missing: Vec<&String> = catalog
        .iter()
        .filter(|t| !covered.contains(t.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "catalog tools without a bridge conformance case — add one to cases() in tests/bridge_conformance.rs: {missing:?}"
    );
    let stale: Vec<&str> = cases
        .iter()
        .filter(|c| !c.extra && !c.gated && !catalog.contains(&c.tool))
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        stale.is_empty(),
        "cases for tools this build lacks (mark feature-gated ones .gated()): {stale:?}"
    );
    let mut names = BTreeSet::new();
    for c in &cases {
        assert!(names.insert(c.name.clone()), "duplicate case {}", c.name);
    }
}

/// Every case: in-process and bridge agree (module table).
#[test]
fn bridge_matches_in_process() {
    let catalog = catalog();
    let queue: Vec<Case> = cases()
        .into_iter()
        .filter(|c| !c.gated || catalog.contains(&c.tool))
        .collect();
    let queue = Arc::new(Mutex::new(queue));
    let failures = Arc::new(Mutex::new(Vec::<String>::new()));
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let queue = Arc::clone(&queue);
            let failures = Arc::clone(&failures);
            std::thread::spawn(move || loop {
                let Some(case) = queue.lock().unwrap().pop() else {
                    break;
                };
                if let Err(e) = run_case(&case) {
                    failures
                        .lock()
                        .unwrap()
                        .push(format!("── {} ──\n{e}", case.name));
                }
            })
        })
        .collect();
    for w in workers {
        w.join().expect("worker panicked");
    }
    let failures = failures.lock().unwrap();
    assert!(
        failures.is_empty(),
        "{} bridge conformance case(s) failed:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
