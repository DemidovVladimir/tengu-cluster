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
//! | Two bridge processes on one sandbox, the same JSON-RPC ids: two orders, never a replay | `two_bridge_sessions_place_two_orders` |
//!
//! | Side | Process (cwd = its workspace) | Env (after `env_clear`) |
//! |---|---|---|
//! | in-process | one `tengu tool call -c <root>/config.toml --agent conf --batch`: a `{"tool", "args", "call_id": <n>}` line per step, one executor | `PATH`, `TMPDIR`, `HOME` + `TENGU_HOME` under the side root, `TENGU_SECRETS_LOADED` naming `XM_CONFORMANCE_SECRET`, the case env |
//! | bridge | one `tengu mcp-bridge`: `initialize`, then a `tools/call` per step with JSON-RPC id `<n>` | the same + `TENGU_CONFIG`, `TENGU_BRIDGE_AGENT=conf`, `TENGU_BRIDGE_WORKSPACE`, `TENGU_BRIDGE_TOOLS` (the agent's tools), `TENGU_BRIDGE_GRANT_WORKSPACE=1` (a `run-agent` step's bridge: workspace root granted like in-process), `TENGU_BRIDGE_MCP_SERVERS` (server names, as the engine writes them) in `[[mcp_servers]]` cases |
//!
//! A `.transcript(..)` case hands both sides the same conversation (a JSON
//! array of messages in `<root>/transcript.json`): `--transcript` in-process,
//! `TENGU_BRIDGE_TRANSCRIPT_FILE` for the bridge — what a Claude Code run
//! gives its bridge. Without one, no tool sees a conversation.
//!
//! Fixture sandbox (`BASE_TOML` + the case's TOML): `[egress] network =
//! "open"`, `allow_hosts = ["127.0.0.1"]` (a hard-coded upstream host is
//! refused, never reached: no network), `audit = false`; `[agents.conf]` with
//! the side's workspace and the case's tools, the default `[agents.main]` on
//! that workspace too (a `[risk]` sandbox needs one on every agent,
//! `config/xmarket.rs`). Upstreams are a loopback
//! `Mock`: a route matches method + path + substrings of target and body and
//! answers inline JSON, a fixture file (`tests/fixtures/…`) or a
//! `getMultipleAccounts` reply built per request from captured accounts
//! (`Reply::Gma`), or an `l2Book` capture stamped now (`Reply::Book`: a
//! live book the paper gate accepts). Tools with a base-URL override reach
//! it through `.scoped(<env>)` (`SOLANA_RPC_URL`, `HL_API_URL`: `POST /info`
//! routes by body `type`, captured replies in `tests/fixtures/hyperliquid/`;
//! `GECKO_API_URL`: GeckoTerminal OHLCV). `market_history` fetches into the
//! side's `<TENGU_HOME>/state/conf/market.db`: its HL case replays the
//! `xyz:TSLA` candle + funding captures moved to the current hours
//! (`recent_tsla_history`: HL's newest-5 000-bars reach), then reads them
//! back; `backtest` runs on those bars (the side's `[backtest]` library and
//! an inline spec) into `<TENGU_HOME>/state/conf/backtests/<run id>/` — a
//! split's holdout hidden, then read (`holdout-reads.jsonl` beside the runs);
//! its read mode (`run_id`) reads a stored run seeded by `.home_file(..)` (a
//! file under the side's `TENGU_HOME`) from `tests/fixtures/xlab/run_conf_rows/`.
//! `.row(..)` seeds the workspace observation store (the opportunity row a
//! paper entry names). `.setup(..)` runs a `tengu` CLI on each side before
//! the first step: `source_evidence` reads a `sources.db` filled by `tengu
//! sources import` of the captured TED pair `ted/search_change_notice.json`
//! (`<TENGU_HOME>/state/conf/sources.db`), as of before and after the change
//! notice. The SOE cases (`soe(..)`) seed the fixture state root
//! `tests/fixtures/soe/state/` under `<TENGU_HOME>/state/conf/`
//! (`[sources] state = "conf"`) with `TENGU_AGENT_NAME = conf` on both
//! sides, as a `run-agent` stage sets it: the stamped records compare alike.
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
//! | ISO-8601 timestamps, extended and basic (`20261001T120034Z`: a backtest run id's stamp, in its text, run-dir path and files) | `<TIME>` |
//! | history day files `YYYYMMDD.db` | `<DAY>.db` |
//! | epoch ms / s within 2 days of now (fixture timestamps stay) | `<EPOCH_MS>` / `<EPOCH_S>` |
//! | JSON-RPC `"id":<n>` (request bodies) | `"id":<N>` |
//! | call-id nonce `mcp:<32 hex>:` (bridge and `tengu tool call`: one per process) | `mcp:<NONCE>:` |
//! | a strategy ranking's `"report_sha256": "<64 hex>"` (the hash of a `report.json` holding its run id) | `<SHA256>` |
//!
//! Add a case: one `case("<tool>", json!({..}))` row in `cases()` plus the
//! TOML its scope needs, `.route(..)` replies and `.ok("…")` / `.err("…")`
//! (`docs/mcp-bridge.md` § Testing). `TENGU_CONFORMANCE_VERBOSE=1` (with
//! `--nocapture`) prints every case's in-process text, stores and requests;
//! `TENGU_CONFORMANCE_ONLY=<text>` runs only the cases whose name holds it.

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
workspace = "{ws}"

[agents.conf]
engine = "openrouter"
model = "anthropic/claude-haiku-4.5"
workspace = "{ws}"
tools = [{tools}]
skill_packages = [{skills}]
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
    /// `[agents.conf] skill_packages` (`.skill(..)`: copied from
    /// `tests/fixtures/skills/<name>` into `<ws>/skills/<name>`).
    skills: Vec<String>,
    /// `.scoped(env)`: `[default_scopes.<t>]` for every agent tool (workspace
    /// store, mock host, `env` readable) and `env` = the mock.
    scope_env: Option<String>,
    env: Vec<(String, String)>,
    /// Workspace files; `{secret}` expanded.
    files: Vec<(String, String)>,
    /// Files under the side's `TENGU_HOME` (`.home_file(..)`: a stored
    /// backtest run a read names), verbatim.
    home_files: Vec<(String, String)>,
    /// Observation rows seeded into the workspace store, stamped now.
    rows: Vec<Value>,
    /// `.setup(..)`: `tengu -c <side config> <args>` run on each side before
    /// the first step (`{fixtures}`, `{ws}`, `{root}` expanded) — the CLI an
    /// operator runs to fill a store (`tengu sources import`); exit 0.
    setup: Vec<Vec<String>>,
    routes: Vec<Route>,
    /// `TENGU_BRIDGE_MCP_SERVERS` handed to the bridge — server names, as
    /// the Claude Code engine writes them (the bridge takes each from the
    /// side's config, like the in-process side).
    mcp_servers: Option<Value>,
    /// The conversation every call sees (`.transcript(..)`): a JSON array
    /// of messages in `<root>/transcript.json` — `--transcript` in-process,
    /// `TENGU_BRIDGE_TRANSCRIPT_FILE` for the bridge, as a Claude Code run
    /// hands its bridge.
    transcript: Option<Value>,
    /// `.sandbox(<name>)`: the config at `<root>/sandboxes/<name>/config.toml`
    /// (a ranking contract names its sandbox), else `<root>/config.toml`.
    sandbox: Option<String>,
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
        skills: Vec::new(),
        scope_env: None,
        env: Vec::new(),
        files: Vec::new(),
        home_files: Vec::new(),
        rows: Vec::new(),
        setup: Vec::new(),
        routes: Vec::new(),
        mcp_servers: None,
        transcript: None,
        sandbox: None,
        gated: false,
        extra: false,
    }
}

impl Case {
    fn named(mut self, variant: &str) -> Self {
        self.name = format!("{}:{variant}", self.tool);
        self
    }
    /// The case covers `tool` (a later step) — named `<tool>`.
    fn retool(mut self, tool: &str) -> Self {
        self.tool = tool.to_string();
        self.name = tool.to_string();
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
    /// The fixture skill `tests/fixtures/skills/<name>` in the workspace's
    /// project tier and in `[agents.conf] skill_packages`.
    fn skill(mut self, name: &str) -> Self {
        self.skills.push(name.to_string());
        let text = fixture(&format!("skills/{name}/SKILL.md"));
        self.file(&format!("skills/{name}/SKILL.md"), &text)
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
    /// A file at `path` under the side's `TENGU_HOME`.
    fn home_file(mut self, path: &str, content: &str) -> Self {
        self.home_files
            .push((path.to_string(), content.to_string()));
        self
    }
    /// A row in `<ws>/.tengu/observations.db` before the first step
    /// (`observed_at_ms` = now).
    fn row(mut self, observation: Value) -> Self {
        self.rows.push(observation);
        self
    }
    /// A `tengu` CLI run on each side before the first step (`Case::setup`).
    fn setup(mut self, args: &[&str]) -> Self {
        self.setup
            .push(args.iter().map(|a| a.to_string()).collect());
        self
    }
    fn route(mut self, r: Route) -> Self {
        self.routes.push(r);
        self
    }
    /// The conversation both sides hand every call (`Case::transcript`).
    fn transcript(mut self, messages: Value) -> Self {
        self.transcript = Some(messages);
        self
    }
    fn gated(mut self) -> Self {
        self.gated = true;
        self
    }
    /// The config as sandbox `name`'s (`Case::sandbox`).
    fn sandbox(mut self, name: &str) -> Self {
        self.sandbox = Some(name.to_string());
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

/// A GeckoTerminal SOL / USDC pool (`docs/xlab-2026-10-01.md` § 4), in full.
const GECKO_POOL: &str = "Gf7sXMoP8iRw4iiXmJ1nq4vxcRycbGXy5RL8a8LnTd3v";
/// Its OHLCV path on the mock (`GECKO_API_URL` = the mock root).
const GECKO_OHLCV_PATH: &str =
    "/networks/solana/pools/Gf7sXMoP8iRw4iiXmJ1nq4vxcRycbGXy5RL8a8LnTd3v/ohlcv/hour";

/// The xlab warehouse (`market.db`) in `<TENGU_HOME>/state/conf/`.
const XLAB_TOML: &str = "[xmarket]\nstate = \"conf\"\n";

/// The `xyz:TSLA` captures (`hyperliquid/candleSnapshot_xyz_TSLA_1h.json`:
/// 67 hourly bars from Fri 2026-09-25 20:00Z; `fundingHistory_xyz_TSLA.json`:
/// 68 settlements) moved by whole hours so the last bar closes two hours
/// before the current one — HL serves only its newest 5 000 bars, so fixed
/// dates would one day fall out of reach and be clamped. Returns the two
/// replies and the window `[from, to)` they fill.
fn recent_tsla_history() -> (String, String, i64, i64) {
    const H: i64 = 3_600_000;
    tsla_history_closing_at(now_ms() / H * H - 2 * H)
}

/// [`recent_tsla_history`] with the last bar closing at `close_ms` (a whole
/// hour).
fn tsla_history_closing_at(close_ms: i64) -> (String, String, i64, i64) {
    const H: i64 = 3_600_000;
    let mut candles: Value =
        serde_json::from_str(&fixture("hyperliquid/candleSnapshot_xyz_TSLA_1h.json")).unwrap();
    let mut funding: Value =
        serde_json::from_str(&fixture("hyperliquid/fundingHistory_xyz_TSLA.json")).unwrap();
    let bars = candles.as_array_mut().unwrap();
    let first = bars[0]["t"].as_i64().unwrap();
    let last = bars[bars.len() - 1]["t"].as_i64().unwrap();
    let shift = (close_ms - H) - last;
    let moved = |v: &Value| json!(v.as_i64().unwrap() + shift);
    for b in bars.iter_mut() {
        b["t"] = moved(&b["t"]);
        b["T"] = moved(&b["T"]);
    }
    for f in funding.as_array_mut().unwrap() {
        f["time"] = moved(&f["time"]);
    }
    (
        candles.to_string(),
        funding.to_string(),
        first + shift,
        last + shift + H,
    )
}

/// `market_history` with `fetch = true` (HL bars + funding through the mock
/// into `market.db`), then the same window read back without a fetch.
fn market_history_hl() -> Case {
    let (candles, funding, from, to) = recent_tsla_history();
    let window = json!({"instrument": "hyperliquid:xyz:TSLA", "interval": "1h",
                        "from": from, "to": to});
    let mut fetch = window.clone();
    fetch["fetch"] = json!(true);
    case("market_history", fetch)
        .toml(XLAB_TOML)
        .scoped("HL_API_URL")
        .route(info("candleSnapshot").has("\"coin\":\"xyz:TSLA\"").json(&candles))
        .route(info("fundingHistory").has("\"coin\":\"xyz:TSLA\"").json(&funding))
        .ok("fetched: 67 bar(s), 67 funding row(s) written from hl:127.0.0.1")
        .then("market_history", window)
        .ok("mkt_history hyperliquid:xyz:TSLA 1h bars=67 <TIME> … <TIME> last_close=360.2 ret_bps=-331.2 | ok <AGE>s live")
}

/// `market_history` with `[backtest.splits]`: a 2-for-1 split 30 bars into
/// the fetched `xyz:TSLA` captures reaches both sides (the sandbox's
/// sections), the bars before it are read adjusted, as a backtest reads
/// them, and the row says so.
fn market_history_splits() -> Case {
    let (candles, funding, from, to) = recent_tsla_history();
    let at = chrono::DateTime::from_timestamp_millis(from + 30 * 3_600_000)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let window = json!({"instrument": "hyperliquid:xyz:TSLA", "interval": "1h",
                        "from": from, "to": to});
    let mut fetch = window.clone();
    fetch["fetch"] = json!(true);
    case("market_history", fetch)
        .named("splits")
        .toml(XLAB_TOML)
        .toml(&format!(
            "[backtest]\n[backtest.splits]\n\"hyperliquid:xyz:TSLA\" = [{{ at = \"{at}\", ratio = \
             2.0 }}]\n"
        ))
        .scoped("HL_API_URL")
        .route(
            info("candleSnapshot")
                .has("\"coin\":\"xyz:TSLA\"")
                .json(&candles),
        )
        .route(
            info("fundingHistory")
                .has("\"coin\":\"xyz:TSLA\"")
                .json(&funding),
        )
        .ok("fetched: 67 bar(s), 67 funding row(s) written from hl:127.0.0.1")
        .then("market_history", window)
        .ok(
            "\nsplit-adjusted hyperliquid:xyz:TSLA: ratio 2 (new shares per old) at <TIME> — 30 \
             bars before it: prices ÷ 2, volume × 2 — [backtest.splits], as a backtest reads the \
             bars",
        )
}

/// `market_history` on a Solana mint: GeckoTerminal pool bars through the
/// mock (`GECKO_API_URL`), then read back.
fn market_history_gecko() -> Case {
    let id = format!("solana:{WSOL}");
    let window = json!({"instrument": id, "from": "2026-09-25T07:00:00Z",
                        "to": "2026-09-25T10:00:00Z"});
    let mut fetch = window.clone();
    fetch["fetch"] = json!(true);
    fetch["pool"] = json!(GECKO_POOL);
    // 07:00 … 09:00 UTC, newest first as Gecko sends them.
    let ohlcv: Vec<Value> = [(9, 117.66), (8, 117.41), (7, 117.52)]
        .iter()
        .map(|(h, c)| {
            json!([
                1_790_319_600 + (h - 7) * 3600,
                117.5,
                117.9,
                117.3,
                c,
                250_000.5
            ])
        })
        .collect();
    let page = json!({"data": {"id": "x", "type": "ohlcv_request_response",
                      "attributes": {"ohlcv_list": ohlcv}}, "meta": {}});
    case("market_history", fetch)
        .named("gecko")
        .toml(XLAB_TOML)
        .scoped("GECKO_API_URL")
        .route(
            get(GECKO_OHLCV_PATH)
                .has(&format!("token={WSOL}"))
                .json(&page.to_string()),
        )
        .ok(&format!(
            "fetched: 3 bar(s) written from gecko:solana:{GECKO_POOL}"
        ))
        .then("market_history", window)
        .ok(&format!(
            "mkt_history {id} 1h bars=3 <TIME> … <TIME> last_close=117.66 ret_bps=11.9"
        ))
}

/// `[backtest]` over the `xyz:TSLA` captures: xyz costs, a one-name
/// universe and a move trigger — a kind without wall-clock instants, so
/// the captures at the current hours trade the same bars whenever it runs.
const BACKTEST_TOML: &str = r#"
[backtest]
bootstrap = 200

[backtest.costs."hyperliquid:xyz:"]
taker_fee_bps = 0.9
half_spread = { model = "fixed", bps = 1.0 }

[backtest.universes]
conf = ["hyperliquid:xyz:TSLA"]

[backtest.strategies.conf_move]
kind = "move_trigger"
universe = "@conf"
interval = "1h"
lookback_bars = 1
threshold_bps = 25
direction = "fade"
hold_bars = 3
"#;

/// The inline spec of the `backtest` case: the library move trigger's
/// placebo (follow), named.
fn conf_follow_spec() -> Value {
    json!({"name": "conf_follow", "kind": "move_trigger", "universe": "@conf",
           "interval": "1h", "lookback_bars": 1, "threshold_bps": 25,
           "direction": "follow", "hold_bars": 3})
}

/// `backtest` on the bars `market_history` fetched through the mock (the HL
/// captures of `market_history_hl`): the library strategy, then an inline
/// spec — rules arms, a run dir each under the side's state dir (its run
/// id's stamp normalised to `<TIME>`).
fn backtest_hl() -> Case {
    let (candles, funding, from, to) = recent_tsla_history();
    let fetch = json!({"instrument": "hyperliquid:xyz:TSLA", "interval": "1h",
                       "from": from, "to": to, "fetch": true});
    case("market_history", fetch)
        .toml(XLAB_TOML)
        .toml(BACKTEST_TOML)
        .scoped("HL_API_URL")
        .route(
            info("candleSnapshot")
                .has("\"coin\":\"xyz:TSLA\"")
                .json(&candles),
        )
        .route(
            info("fundingHistory")
                .has("\"coin\":\"xyz:TSLA\"")
                .json(&funding),
        )
        .ok("fetched: 67 bar(s), 67 funding row(s) written from hl:127.0.0.1")
        .then(
            "backtest",
            json!({"strategy": "conf_move", "from": from, "to": to}),
        )
        .ok("backtest <TIME>-conf_move conf_move move_trigger 1h research n=2 mean_net_bps=-27.39")
        .then(
            "backtest",
            json!({"spec": conf_follow_spec(), "from": from, "to": to}),
        )
        .ok("backtest <TIME>-conf_follow conf_follow move_trigger 1h research n=2 mean_net_bps=")
        .retool("backtest")
}

/// `backtest`'s holdout on the HL captures (`backtest_hl`'s fetch): a split
/// between the move trigger's two trades (the captures' 2026-09-27 00:00Z,
/// moved like them) runs the in-sample half only — the first trade —; with
/// `holdout: true` both halves run and each read is a line of
/// `<TENGU_HOME>/state/conf/backtests/holdout-reads.jsonl` (#1 of each spec,
/// the split's 1st then 2nd), alike on both sides. Distinct spec names: no
/// two runs share a run id's second.
fn backtest_holdout() -> Case {
    let (candles, funding, from, to) = recent_tsla_history();
    let split = format!("time:{}", from + 28 * 3_600_000);
    let fetch = json!({"instrument": "hyperliquid:xyz:TSLA", "interval": "1h",
                       "from": from, "to": to, "fetch": true});
    let mut hidden = conf_follow_spec();
    hidden["name"] = json!("conf_hidden");
    hidden["direction"] = json!("fade");
    case("market_history", fetch)
        .toml(XLAB_TOML)
        .toml(BACKTEST_TOML)
        .scoped("HL_API_URL")
        .route(
            info("candleSnapshot")
                .has("\"coin\":\"xyz:TSLA\"")
                .json(&candles),
        )
        .route(
            info("fundingHistory")
                .has("\"coin\":\"xyz:TSLA\"")
                .json(&funding),
        )
        .ok("fetched: 67 bar(s), 67 funding row(s) written from hl:127.0.0.1")
        .then(
            "backtest",
            json!({"spec": hidden, "from": from, "to": to, "split": split}),
        )
        .ok(
            "\nholdout hidden: split time:<TIME> — no decision at or after the split (<TIME>): no \
             holdout trade was simulated",
        )
        .then(
            "backtest",
            json!({"strategy": "conf_move", "from": from, "to": to, "split": split,
                   "holdout": true}),
        )
        .ok("\nholdout read #1 for this spec · 1 read(s) of split time:<TIME> in this sandbox")
        .then(
            "backtest",
            json!({"spec": conf_follow_spec(), "from": from, "to": to, "split": split,
                   "holdout": true}),
        )
        .ok("\nholdout read #1 for this spec · 2 read(s) of split time:<TIME> in this sandbox")
        .retool("backtest")
        .named("holdout")
}

/// A stored run's id (`tests/fixtures/xlab/run_conf_rows/`: a holdout read
/// of the matrix move trigger, split `time:2026-09-27`, one trade a half).
const ROWS_RUN: &str = "20261001T182112Z-conf_rows";

/// `backtest` with `run_id`: the seeded split run's rows by run id — its
/// holdout trade hidden, then shown on purpose (a read, counted: `via =
/// "rows"`); a path and an unknown id refused alike.
fn backtest_rows() -> Case {
    let dir = format!("state/conf/backtests/{ROWS_RUN}");
    let mut c = case("backtest", json!({"run_id": ROWS_RUN, "view": "trades"}))
        .named("rows")
        .toml(XLAB_TOML)
        .toml(BACKTEST_TOML);
    for f in [
        "report.json",
        "trades-research.jsonl",
        "trades-capped.jsonl",
    ] {
        c = c.home_file(
            &format!("{dir}/{f}"),
            &fixture(&format!("xlab/run_conf_rows/{f}")),
        );
    }
    c.ok("\nsplit time:<TIME>: holdout hidden — its 1 trade(s) left out")
        .then(
            "backtest",
            json!({"run_id": ROWS_RUN, "view": "periods", "holdout": true}),
        )
        .ok("both halves shown (1 holdout trade(s)) · holdout read #1 for this spec")
        .then("backtest", json!({"run_id": "../conf/market.db"}))
        .err("'run_id' `../conf/market.db` is not a run id")
        .then(
            "backtest",
            json!({"run_id": "20200101T000000Z-nope", "view": "notes"}),
        )
        .err("no run `<TIME>-nope` in the state dir's backtests/ (the newest: <TIME>-conf_rows)")
}

/// The SOE fixture runs (`tests/fixtures/soe/state/`): a live week open in
/// `PROPOSE` and a replay open in `CHALLENGE`.
const SOE_RUN: &str = "cycles/2026-W42";
const SOE_REPLAY: &str = "replays/fixture.w42";

/// An SOE case: `[sources] state = "conf"` and the fixture state root
/// (written by `tools::soe::tests::fixture_state_is_current`) under the
/// side's `<TENGU_HOME>/state/conf/`; `TENGU_AGENT_NAME` = the agent, as a
/// `run-agent` stage sets it (its bridge inherits it).
fn soe(c: Case) -> Case {
    let root = Path::new(FIXTURES).join("soe/state");
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    let mut c = c
        .toml("[sources]\nstate = \"conf\"\n")
        .env("TENGU_AGENT_NAME", AGENT);
    for f in files {
        let rel = f
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        c = c.home_file(
            &format!("state/conf/{rel}"),
            &fixture(&format!("soe/state/{rel}")),
        );
    }
    c
}

/// A fixture draft (`tests/fixtures/soe/drafts/`).
fn soe_draft(name: &str) -> Value {
    serde_json::from_str(&fixture(&format!("soe/drafts/{name}"))).unwrap()
}

/// `soe_challenge`'s flat fixture arguments on `run`, aimed at `target`.
fn soe_challenge_args(run: &str, target: &str) -> Value {
    let mut a = soe_draft("challenge.json");
    a["run"] = run.into();
    a["target"] = target.into();
    a
}

/// `[sources]` with one enabled `ted_search` row (synthetic reviewed terms;
/// its host the loopback the fixture egress allows — nothing is fetched)
/// and `source_evidence`'s workspace scope: `sources.db` lands in
/// `<TENGU_HOME>/state/conf/`.
const SOURCES_TOML: &str = r#"
[sources]
state = "conf"

[sources.registry.ted_search]
kind = "ted_search"
class = "law_regulator"
trust = "primary"
revision = "immutable"
enabled = true
hosts = ["127.0.0.1"]
auth = "none"
rate_limit = "ted"
store_raw = true
jurisdiction = "EU"
language = "en"
query = "publication-date >= {from} AND publication-date <= {to}"
license = "synthetic conformance terms"
terms_url = "https://example.org/terms"
terms_sha256 = "6e81b3dc57dee4be066314f20ab0be61c23469c5ad56743239ee1bd399d8899a"
terms_reviewed_at = "2026-10-08"
raw_retention_days = 90
record_retention_days = 0

[rate_limits.ted]
per_minute = 60
burst = 2

[default_scopes.source_evidence]
fs_roots = ["{ws}"]
"#;

/// `source_evidence` on the captured TED pair of `ted/search_change_notice.json`
/// (657981-2026 and its change notice 674231-2026), imported into each
/// side's `sources.db` by `tengu sources import` (the operator's CLI) as read
/// 2026-10-02: knowable at 2026-09-30 the original stands; at 2026-10-03 the
/// change supersedes it (`correction`). No fetch argument exists.
fn source_evidence_asof() -> Case {
    let at = |day: &str| json!({"at": day, "mode": "knowable", "source": "ted_search"});
    case("source_evidence", at("2026-09-30"))
        .named("asof")
        .toml(SOURCES_TOML)
        .setup(&[
            "sources",
            "import",
            "--source",
            "ted_search",
            "--file",
            "{fixtures}/ted/search_change_notice.json",
            "--observed-at",
            "2026-10-02T08:00:00Z",
        ])
        .ok("source_asof <TIME> knowable: 1 facts · 0 pending")
        .then("source_evidence", at("2026-10-03"))
        .ok("\nsuperseded ted_search:657981-2026:")
        .then(
            "source_evidence",
            json!({"at": "2026-10-03", "fetch": true}),
        )
        .err("unknown argument(s) [\"fetch\"]")
}

/// The `strategy_ranking` case's sandbox: the fixture contract
/// `rank.test.v1` (`tests/fixtures/strategy_ranking/lineage`, sealed; it
/// names sandbox `rank-test`, so the config sits at
/// `<root>/sandboxes/rank-test/config.toml`) and its two move triggers on
/// `xyz:TSLA` (`tests/fixtures/strategy_ranking/config.toml`'s library).
const RANK_TOML: &str = r#"
[backtest]
bootstrap = 200
seed = 7

[backtest.costs."hyperliquid:xyz:"]
taker_fee_bps = 0.9
half_spread = { model = "fixed", bps = 1.0 }

[backtest.universes]
tsla = ["hyperliquid:xyz:TSLA"]

[backtest.strategies.rank_fade]
kind = "move_trigger"
universe = "@tsla"
interval = "1h"
lookback_bars = 1
threshold_bps = 25
direction = "fade"
hold_bars = 3

[backtest.strategies.rank_follow]
kind = "move_trigger"
universe = "@tsla"
interval = "1h"
lookback_bars = 1
threshold_bps = 25
direction = "follow"
hold_bars = 3

[strategy_ranking]
registry = "{fixtures}/strategy_ranking/lineage"
contracts = ["rank.test.v1"]
"#;

/// `strategy_ranking` on the HL captures fetched through the mock, moved so
/// their last bar closes at the contract's newest past cutoff (15:00 UTC):
/// the date's ranking runs both move triggers (a run dir each, the files
/// under `<TENGU_HOME>/state/conf/strategy-rankings/`, `report_sha256`
/// normalised); a rerun returns the published date; `latest` reads it; a
/// contract not listed is refused alike.
fn strategy_ranking_hl() -> Case {
    const H: i64 = 3_600_000;
    const DAY: i64 = 24 * H;
    let today_15 = now_ms() / DAY * DAY + 15 * H;
    let cutoff = if today_15 <= now_ms() {
        today_15
    } else {
        today_15 - DAY
    };
    let date = chrono::DateTime::from_timestamp_millis(cutoff)
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    let (candles, funding, from, to) = tsla_history_closing_at(cutoff);
    let fetch = json!({"instrument": "hyperliquid:xyz:TSLA", "interval": "1h",
                       "from": from, "to": to, "fetch": true});
    let run = json!({"action": "run", "date": date});
    case("market_history", fetch)
        .sandbox("rank-test")
        .toml(XLAB_TOML)
        .toml(RANK_TOML)
        .scoped("HL_API_URL")
        .route(
            info("candleSnapshot")
                .has("\"coin\":\"xyz:TSLA\"")
                .json(&candles),
        )
        .route(
            info("fundingHistory")
                .has("\"coin\":\"xyz:TSLA\"")
                .json(&funding),
        )
        .ok("fetched: 67 bar(s), 67 funding row(s) written from hl:127.0.0.1")
        .then("strategy_ranking", run.clone())
        .ok(&format!(
            "strategy_ranking run rank.test.v1 {date} COMPLETE · ran now · latest replaced · 2 \
             ranked, 0 ineligible, 0 failed, 0 dropped\nstrategy ranking `rank.test.v1` {date}: \
             COMPLETE · contract sha256 \
             28c5d0c0339d82f13d14949dfebe2d423adc17a70aee292cbce55549d335f60d\n"
        ))
        .then("strategy_ranking", run)
        .ok(
            "\ncohort 1 of 1, weakest → strongest:\n  1. rank_fade  ci95_lo -56.86  mean -27.39  \
             n 2  run:conf/<TIME>-rank_fade\n  2. rank_follow  ci95_lo -9.67  mean +19.79  n 2  \
             run:conf/<TIME>-rank_follow\n",
        )
        .then("strategy_ranking", json!({"action": "latest"}))
        .ok(&format!(
            "strategy_ranking latest rank.test.v1 {date} COMPLETE · the newest COMPLETE ranking — \
             nothing ran"
        ))
        .then(
            "strategy_ranking",
            json!({"action": "run", "contract": "rank.nope"}),
        )
        .err("contract_not_listed: `rank.nope` is not in [strategy_ranking] contracts (rank.test.v1)")
        .retool("strategy_ranking")
}

/// `[xmarket]` + the $100 `[risk]` / `[paper]` budget (tracker § 7 #3): the
/// ledger lands in `<TENGU_HOME>/state/conf/ledger.db`; the kill-switch file
/// sits outside the workspace (absent); Privy signing off (required with
/// `[risk]`); a 50 ms paper latency without jitter (both sides alike).
const XM_RISK_TOML: &str = r#"
[xmarket]
state = "conf"

[risk]
account = "conf"
mode = "paper"
venues = ["hyperliquid"]
min_lifecycle = "paper_tradable"
instruments_allow = ["hyperliquid:xyz:TSLA"]
instruments_deny = []
max_order_notional_usd = 25
max_position_notional_usd = 50
max_asset_exposure_usd = 50
max_venue_exposure_usd = 100
max_gross_exposure_usd = 100
max_net_exposure_usd = 100
max_leverage = 1
daily_loss_limit_usd = 10
total_loss_limit_usd = 25
min_edge_bps = 10
max_slippage_bps = 30
min_depth_usd = 250
require_hedge_for = ["convergence"]
max_skew_ms = 5000
max_orders_per_min = 6
max_open_orders = 4
kill_switch_file = "{root}/KILL"
allow_reduce_degraded = true

[risk.max_data_age_ms]
book = 5000
ctx = 20000
reference = 60000
quote = 20000

[risk.exits]
take_profit_bps = 200
stop_loss_bps = 100
max_hold_secs = 86400

[paper]
initial_cash_usd = 100
latency_ms = 50
latency_jitter_ms = 0
fee_tier = 0
staking_discount_pct = 0
order_types = ["market", "ioc"]

[default_scopes.sign_and_send_transaction]

[default_scopes.sign_message]
"#;

/// The opportunity row a paper entry names (`min_edge`): 25 bps after costs,
/// backing a buy (`overreaction`).
const OPP_KEY: &str = "xm_compare/1:hyperliquid:xyz:TSLA:hyperliquid:xyz:TSLA";

fn opportunity_row() -> Value {
    json!({
        "key": OPP_KEY, "schema": "xm_compare/1", "tool": "xm_compare",
        "observed_at_ms": 0, "ttl_ms": 600_000, "source": "live", "status": "ok",
        "headline": "compare hyperliquid:xyz:TSLA edge_after_costs_bps=25",
        "features": {"edge_after_costs_bps": 25.0, "side": "buy", "strategy": "overreaction"},
        "data": null
    })
}

/// A $25 market buy of `xyz:TSLA` naming the opportunity row.
fn paper_buy() -> Value {
    json!({"instrument": "hyperliquid:xyz:TSLA", "side": "buy", "notional_usd": 25,
           "kind": "market", "max_slippage_bps": 30, "strategy": "overreaction",
           "opportunity": OPP_KEY})
}

/// A paper case: `hl_ctx` first (the `mkt_ctx/1` + `mkt_instrument/1`
/// rows the gate and the fill read), the $100 budget, the live book (its
/// `time` = now) on the mock, the opportunity row seeded.
fn paper(c: Case) -> Case {
    hl_xyz(c.before("hl_ctx", json!({"coins": ["xyz:TSLA"]})))
        .toml(XM_RISK_TOML)
        .scoped("HL_API_URL")
        .route(
            info("l2Book")
                .has("\"coin\":\"xyz:TSLA\"")
                .book("hyperliquid/l2Book_xyz_TSLA.json"),
        )
        .row(opportunity_row())
}

/// `[xmarket.weekend_fade]` (rule W on `xyz:TSLA`, the $100 `[risk]` of
/// [`XM_RISK_TOML`]) + the recorder, on a calendar whose current Saturday +
/// Sunday break spans three weeks (every weekday from yesterday a
/// holiday): whenever the case runs, the window is before its entry, so
/// `xm_weekend_fade` reports `waiting` on both sides.
fn weekend_fade_toml() -> String {
    use chrono::Datelike;
    let today = chrono::Utc::now().date_naive();
    let holidays: Vec<String> = (-1..=20)
        .map(|d| today + chrono::Duration::days(d))
        .filter(|d| d.weekday().number_from_monday() <= 5)
        .map(|d| format!("\"{}\"", d.format("%Y-%m-%d")))
        .collect();
    format!(
        r#"
[xmarket.calendars.fade]
kind = "exchange"
tz = "America/New_York"
core = ["09:30", "16:00"]
holidays = [{}]

[xmarket.weekend_fade]
calendar = "fade"
universe = ["hyperliquid:xyz:TSLA"]
exclude = []
capped_top_n = 1
min_abs_signal_bps = 50
capped_notional_usd = 25
shadow_account = "conf-shadow"
shadow_initial_cash_usd = 1000
shadow_notional_usd = 25
expected_edge_bps = 23
anchor_max_age_secs = 600
entry_max_age_secs = 120
entry_lateness_max_secs = 600
max_slippage_bps = 100

[recorder]
enabled = true
schemas = ["mkt_ctx/1"]
"#,
        holidays.join(", ")
    )
}

/// A skill in the project tier (`<cwd>/skills/demo`).
/// `a2a` cases: the remote `peer` is the mock (card at its root, JSON-RPC
/// at `/rpc`).
const A2A_REMOTE: &str = "[a2a.remotes.peer]\nurl = \"{mock}\"\nendpoint_url = \"{mock}/rpc\"\n\n[default_scopes.a2a]\nnet_hosts = [\"127.0.0.1\"]\n";
const A2A_CARD: &str = r#"{"name":"Peer","description":"The conformance peer.","version":"1.0.0","supportedInterfaces":[{"url":"http://127.0.0.1/rpc","protocolBinding":"JSONRPC","protocolVersion":"1.0"}],"capabilities":{},"defaultInputModes":["text/plain"],"defaultOutputModes":["text/plain"],"skills":[]}"#;
const A2A_TASK: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"task":{"id":"task-conf-1","contextId":"ctx-conf-1","status":{"state":"TASK_STATE_COMPLETED"},"artifacts":[{"artifactId":"a-1","name":"response","parts":[{"text":"pong from the remote"}]}]}}}"#;
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
        // The run's conversation (a Claude Code run's transcript, `tengu
        // tool call --transcript`), from message 1 on: two fixtures — the
        // first ask with its `list_directory`, the save request with the
        // `skill_distill` call itself — in `evals/prompts.yaml` on both sides.
        case(
            "skill_distill",
            json!({"name": "distilled", "description": "Distilled by the conformance test.",
                   "body_markdown": "# distilled\n\nBody.\n", "metrics": [],
                   "from_message_index": 1, "tier": "workspace"}),
        )
        .transcript(json!([
            {"role": "system", "content": "You are under test."},
            {"role": "user", "content": "list the workspace"},
            {"role": "assistant", "content": "",
             "tool_calls": [{"id": "toolu_1", "name": "list_directory", "arguments": {"path": "."}}]},
            {"role": "tool", "content": "skills/", "tool_call_id": "toolu_1"},
            {"role": "assistant", "content": "One directory: skills/."},
            {"role": "user", "content": "save this dialog as a skill"}
        ]))
        .ok("\"fixtures_created\":2")
        .then("view_skill", json!({"action": "read", "skill": "distilled"}))
        .ok("Distilled by the conformance test."),
        // No conversation handed over: refused on both sides, nothing written
        // (never a skill with `fixtures: []`).
        case(
            "skill_distill",
            json!({"name": "distilled", "description": "Distilled by the conformance test.",
                   "body_markdown": "# distilled\n\nBody.\n", "metrics": [],
                   "from_message_index": 0, "tier": "workspace"}),
        )
        .named("no_conversation")
        .err("no conversation to distill"),
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
        // ── a2a (another agent harness: the mock is the remote) ────────
        case("a2a", json!({"action": "send", "remote": "peer", "message": "ping"}))
            .toml(A2A_REMOTE)
            .route(get("/.well-known/agent-card.json").json(A2A_CARD))
            .route(
                post("/rpc")
                    .has("\"method\":\"SendMessage\"")
                    .has("\"text\":\"ping\"")
                    .json(A2A_TASK),
            )
            .ok("artifact response a-1:\npong from the remote"),
        case("a2a", json!({"action": "list"}))
            .named("list")
            .toml(A2A_REMOTE)
            .ok("- peer  http://127.0.0.1:"),
        // A card naming another host is refused before any call to it.
        case("a2a", json!({"action": "card", "remote": "peer"}))
            .named("pinned_host")
            .toml("[a2a.remotes.peer]\nurl = \"{mock}\"\n\n[default_scopes.a2a]\nnet_hosts = [\"127.0.0.1\"]\n")
            .route(get("/.well-known/agent-card.json").json(
                &A2A_CARD.replace("http://127.0.0.1/rpc", "https://elsewhere.example.com/rpc"),
            ))
            .err("tengu calls only the configured hosts"),
        // ── crypto (Privy is not configured in tests) ──────────────────
        case("sign_and_send_transaction", json!({"to": WALLET, "value": "1"}))
            .err("PRIVY_APP_ID"),
        case("sign_message", json!({"message": "hello"})).err("PRIVY_APP_ID"),
        case("get_wallet_address", json!({})).err("PRIVY_APP_ID"),
        // Privy requests pass `[egress]` like `http_request`: api.privy.io
        // is outside `allow_hosts`, nothing is sent.
        case("get_wallet_address", json!({}))
            .named("egress_denied")
            .env("PRIVY_APP_ID", "conformance-app")
            .env("PRIVY_APP_SECRET", "conformance-app-secret")
            .env("PRIVY_WALLET_ID", "conformance-wallet")
            .err("not in allow_hosts"),
        // A configured scope gates the Privy env like any networked tool's.
        case("get_wallet_address", json!({}))
            .named("scope")
            .toml("[default_scopes.get_wallet_address]\nwallets = [\"default\"]\nnet_hosts = [\"api.privy.io\"]\n")
            .err("env var 'PRIVY_APP_ID' not in allowed env_reads"),
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
        case(
            "lp_swap_plan",
            json!({"wallet_sol": 0.5, "wallet_usdc": 1000,
                   "target_sol": 1, "target_usdc": 100,
                   "permanent_minimum_sol": 0.2, "rent_reserve_sol": 0.1,
                   "current_price": 100, "slippage_buffer_pct": 0.02,
                   "context": "rebalance"}),
        )
        .ok("direction=usdc_to_sol"),
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
        // Book + last trade: 20 levels a side stored in `data`.
        case(
            "hl_book",
            json!({"coin": "xyz:TSLA", "include_trades": true}),
        )
        .scoped("HL_API_URL")
        .route(
            info("l2Book")
                .has("\"coin\":\"xyz:TSLA\"")
                .file("hyperliquid/l2Book_xyz_TSLA.json"),
        )
        .route(
            info("recentTrades")
                .has("\"coin\":\"xyz:TSLA\"")
                .file("hyperliquid/recentTrades_xyz_TSLA.json"),
        )
        .ok("book hyperliquid:xyz:TSLA bid=347.94 ask=347.97"),
        // ── xmarket risk: `[risk]` + `[paper]` + the ledger reach the bridge
        case("risk_status", json!({}))
            .toml(XM_RISK_TOML)
            .ok("risk account=conf halt=none equity=100.00 daily_pnl=0.00 total_pnl=0.00 headroom=10.00"),
        // Without `[risk]`: refused alike (tracker convention 9).
        case("risk_status", json!({}))
            .named("no_risk")
            .err("risk_config_missing"),
        // ── xmarket exec tools: the gate inside the tool, both sides alike ──
        // Allowed: $25 → 0.071 at the ask 347.97 after the latency.
        paper(case("paper_order", paper_buy()))
            .ok("paper_fill filled buy hyperliquid:xyz:TSLA qty=0.071"),
        // No opportunity row named: one verdict row, no order.
        paper(case("paper_order", {
            let mut a = paper_buy();
            a.as_object_mut().unwrap().remove("opportunity");
            a
        }))
        .named("denied")
        .ok("paper_fill denied buy hyperliquid:xyz:TSLA notional=25.00 risk=deny rule=missing:edge_after_costs_bps"),
        // Review #8: the row backs a buy — a sell naming it has no edge.
        paper(case("paper_order", {
            let mut a = paper_buy();
            a["side"] = json!("sell");
            a
        }))
        .named("wrong_side")
        .ok("paper_fill denied sell hyperliquid:xyz:TSLA notional=25.00 risk=deny rule=min_edge"),
        // A `require_hedge_for` entry is refused until hedge legs are placed.
        paper(case("paper_order", {
            let mut a = paper_buy();
            a["strategy"] = json!("convergence");
            a
        }))
        .named("hedge")
        .err("hedge_not_supported: the order names strategy `convergence`"),
        paper(case("paper_order", paper_buy()))
            .then(
                "paper_close",
                json!({"instrument": "hyperliquid:xyz:TSLA", "max_slippage_bps": 50}),
            )
            .ok("paper_fill filled sell hyperliquid:xyz:TSLA qty=0.071")
            .retool("paper_close"),
        paper(case("paper_order", paper_buy()))
            .then("paper_positions", json!({}))
            .ok("paper_positions account=conf open=1")
            .retool("paper_positions"),
        // x-exit-rules: a position past its deadline is closed through the
        // same gate under its deterministic exit id.
        paper(case("paper_order", {
            let mut a = paper_buy();
            a["exit_at_ms"] = json!(1_000_000_000_000_i64);
            a
        }))
        .then("xm_exits", json!({}))
        .ok("xm_exits account=conf open=1 due=1 closed=1 failed=0 deadline hyperliquid:xyz:TSLA filled")
        .retool("xm_exits"),
        // Nothing open: a check that places nothing.
        paper(case("xm_exits", json!({})))
            .named("idle")
            .ok("xm_exits account=conf open=0 due=0 closed=0 failed=0"),
        // x-weekend-fade-strategy: `[xmarket.weekend_fade]`, the calendar
        // and the recorder reach the bridge; before the entry the step
        // opens both ledger accounts and reports waiting.
        case("xm_weekend_fade", json!({}))
            .toml(XM_RISK_TOML)
            .toml(&weekend_fade_toml())
            .scoped("HL_API_URL")
            .ok(" waiting next_entry_s=<COUNTDOWN> universe=1 excluded=0 | ok <AGE>s live"),
        // ── xlab research reads: `[xmarket]` → `market.db` in the state dir
        market_history_hl(),
        market_history_gecko(),
        market_history_splits(),
        // Without `[xmarket]` there is no warehouse: refused alike.
        case(
            "market_history",
            json!({"instrument": "hyperliquid:xyz:TSLA"}),
        )
        .named("no_xmarket")
        .err("state_dir_missing: market data unavailable: no [xmarket] section"),
        backtest_hl(),
        // A split's holdout hidden by default, read on purpose and counted;
        // a stored run's rows by run id.
        backtest_holdout(),
        backtest_rows(),
        // A sealed ranking contract's date: run, rerun (published), latest.
        strategy_ranking_hl(),
        // A bad inline spec: refused alike, every problem named, no run dir.
        case(
            "backtest",
            json!({"spec": {"kind": "move_trigger", "universe": "@conf", "interval": "1h",
                            "lookback_bars": 0, "threshold_bps": 0, "direction": "fade",
                            "hold_bars": 0}}),
        )
        .named("bad_spec")
        .toml(XLAB_TOML)
        .toml(BACKTEST_TOML)
        .err(
            "backtest: spec refused — 3 problem(s), fix each and call again:\n\
             strategy `architect_spec`: lookback_bars must be within 1..=10000\n\
             strategy `architect_spec`: threshold_bps must be finite, > 0 and ≤ 10000\n\
             strategy `architect_spec`: hold_bars must be within 1..=10000",
        ),
        // `[xmarket]` without `[backtest]`: refused alike.
        case("backtest", json!({"strategy": "conf_move"}))
            .named("no_backtest")
            .toml(XLAB_TOML)
            .err("backtest_config_missing: backtests unavailable: no [backtest] section"),
        // ── O2 source evidence: `[sources]` → `sources.db` in the state dir
        source_evidence_asof(),
        // Without `[sources]` there is no source store: refused alike.
        case("source_evidence", json!({}))
            .named("no_sources")
            .err("sources_state_missing: no [sources] section"),
        // ── SOE (O3): the fixture state root under <TENGU_HOME>/state/conf
        soe(case("soe_view", json!({"run": SOE_RUN})))
            .ok("soe_view cycles/2026-W42 head OPEN PROPOSE decided_at=<TIME> shown=1 of 1 from 0 | ok")
            .then("soe_view", json!({"run": SOE_RUN, "view": "candidates"}))
            .ok(" · news-automation v1 · AUTOMATE · revenue RECURRING · carried from 2026-W41 · ")
            .then("soe_view", json!({"run": SOE_RUN, "view": "history"}))
            .ok("## 2026-W41.e01 · news-automation v1 · decided <TIME> · verdict HOLD")
            .then("soe_view", json!({"run": SOE_RUN, "view": "packet", "limit": 1}))
            .ok("more: 3 of 4 left")
            .then("soe_view", json!({"run": "cycles/2026-W01"}))
            .err("run_not_found: cycles/2026-W01"),
        soe(case(
            "soe_propose",
            json!({"run": SOE_RUN, "proposal": soe_draft("proposal.json")}),
        ))
        .ok("soe_propose 2026-W42.p01 news-automation v1 AUTOMATE run=cycles/2026-W42 | written")
        .then(
            "soe_propose",
            json!({"run": SOE_RUN, "proposal": soe_draft("proposal.json")}),
        )
        .err("- duplicate: opportunity `news-automation` is already proposal `2026-W42.p01`"),
        soe(case("soe_challenge", soe_challenge_args(SOE_REPLAY, "news-automation")))
            .ok(
                "soe_challenge 2026-W42.c01 target=news-automation kind=HIDDEN_LABOR effect=WIDEN \
                 economics.owner_hours_per_month run=replays/fixture.w42 | written",
            )
            .then("soe_challenge", soe_challenge_args(SOE_REPLAY, "nobody"))
            .err("- unknown_target:")
            .then("soe_challenge", soe_challenge_args(SOE_RUN, "news-automation"))
            .err("- stage_closed: cycles/2026-W42 is in phase Propose"),
        // No `[sources]`: no state root, refused alike.
        case("soe_view", json!({"run": SOE_RUN}))
            .named("no_sources")
            .err("soe_state_missing: no [sources] section"),
    ];
    // ── [[mcp_servers]] proxy tool (not a catalog row) ─────────────────
    let mut proxy = case("fake__echo", json!({}))
        .toml(&format!(
            "[[mcp_servers]]\nname = \"fake\"\ntransport = \"stdio\"\ncommand = [\"sh\", \"{fake_server}\"]\n"
        ))
        .ok("pong");
    proxy.extra = true;
    // Names only, as the Claude Code engine writes them: the bridge takes
    // the server from the side's config.
    proxy.mcp_servers = Some(json!(["fake"]));
    cases.push(proxy);
    // A server tool outside the agent's `tools` runs on neither side.
    let mut unlisted = case("fake__echo", json!({}))
        .named("not_listed")
        .tools(&["read_file"])
        .toml(&format!(
            "[[mcp_servers]]\nname = \"fake\"\ntransport = \"stdio\"\ncommand = [\"sh\", \"{fake_server}\"]\n"
        ))
        .err("not available to this agent");
    unlisted.extra = true;
    unlisted.mcp_servers = Some(json!(["fake"]));
    cases.push(unlisted);
    // ── shell skill (not a catalog row): `skill_packages` loads it on both
    //    sides (`tests/fixtures/skills/matrix_cat`); output redacted alike
    let mut skill = case("matrix_cat", json!({"path": "note.txt"}))
        .skill("matrix_cat")
        .file("note.txt", "skill {secret}\n")
        .ok("skill [REDACTED]");
    skill.extra = true;
    cases.push(skill);
    // A `[risk]` sandbox runs no shell: the skill loads on neither side.
    let mut no_shell = case("matrix_cat", json!({"path": "note.txt"}))
        .named("risk")
        .skill("matrix_cat")
        .toml(XM_RISK_TOML)
        .file("note.txt", "x")
        .err("not available to this agent");
    no_shell.extra = true;
    cases.push(no_shell);
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
    /// An HL `l2Book` fixture with its `time` set to now: a live book.
    Book(&'static str),
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
    fn book(mut self, path: &'static str) -> Self {
        self.reply = Reply::Book(path);
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// The workspace observation store's table (`outbound/observations.rs`)
/// with `rows`, each stamped now.
fn seed_rows(ws: &Path, rows: &[Value]) {
    if rows.is_empty() {
        return;
    }
    std::fs::create_dir_all(ws.join(".tengu")).unwrap();
    let conn = rusqlite::Connection::open(ws.join(".tengu/observations.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS observations (
           key TEXT PRIMARY KEY, schema TEXT NOT NULL, observed_at_ms INTEGER NOT NULL,
           slot INTEGER, ttl_ms INTEGER NOT NULL, status TEXT NOT NULL, body TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS observations_schema ON observations(schema, observed_at_ms);",
    )
    .unwrap();
    let now = now_ms();
    for row in rows {
        let mut row = row.clone();
        row["observed_at_ms"] = json!(now);
        conn.execute(
            "INSERT INTO observations(key, schema, observed_at_ms, slot, ttl_ms, status, body)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6)",
            rusqlite::params![
                row["key"].as_str().unwrap(),
                row["schema"].as_str().unwrap(),
                now,
                row["ttl_ms"].as_i64().unwrap(),
                row["status"].as_str().unwrap(),
                row.to_string()
            ],
        )
        .unwrap();
    }
}

fn fixture(path: &str) -> String {
    std::fs::read_to_string(Path::new(FIXTURES).join(path))
        .unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

fn render_reply(reply: &Reply, request_body: &str) -> String {
    match reply {
        Reply::Json(s) => s.clone(),
        Reply::File(p) => fixture(p),
        Reply::Book(p) => {
            let mut book: Value = serde_json::from_str(&fixture(p)).unwrap();
            book["time"] = json!(now_ms());
            book.to_string()
        }
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
        for (path, content) in &case.home_files {
            let p = root.join("home/.tengu").join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        seed_rows(&ws, &case.rows);
        let tools: Vec<String> = case
            .agent_tools()
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect();
        let skills: Vec<String> = case.skills.iter().map(|s| format!("\"{s}\"")).collect();
        let toml = format!("{BASE_TOML}{}", case.full_toml())
            .replace("{tools}", &tools.join(", "))
            .replace("{skills}", &skills.join(", "));
        let config = match &case.sandbox {
            Some(name) => {
                let dir = root.join("sandboxes").join(name);
                std::fs::create_dir_all(&dir).unwrap();
                dir.join("config.toml")
            }
            None => root.join("config.toml"),
        };
        std::fs::write(&config, expand(&toml, &root, &ws, mock)).unwrap();
        if let Some(messages) = &case.transcript {
            std::fs::write(root.join("transcript.json"), messages.to_string()).unwrap();
        }
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

    /// Each `Case::setup` run, with this side's config and env; exit 0.
    fn setup(&self, case: &Case, mock: &str) -> Result<(), String> {
        for args in &case.setup {
            let args: Vec<String> = args
                .iter()
                .map(|a| expand(a, &self.root, &self.ws, mock))
                .collect();
            let out = self
                .command(case, mock)
                .arg("-c")
                .arg(&self.config)
                .args(&args)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("spawn tengu {args:?}: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "setup tengu {args:?} failed:\n{}\n{}",
                    String::from_utf8_lossy(&out.stdout),
                    tail(&String::from_utf8_lossy(&out.stderr))
                ));
            }
        }
        Ok(())
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
    if case.transcript.is_some() {
        cmd.arg("--transcript")
            .arg(side.root.join("transcript.json"));
    }
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
        .env("TENGU_BRIDGE_GRANT_WORKSPACE", "1");
    if let Some(servers) = &case.mcp_servers {
        cmd.env("TENGU_BRIDGE_MCP_SERVERS", servers.to_string());
    }
    if case.transcript.is_some() {
        cmd.env(
            "TENGU_BRIDGE_TRANSCRIPT_FILE",
            side.root.join("transcript.json"),
        );
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
            // Basic format: the UTC second a backtest run id embeds.
            (r"\b\d{8}T\d{6}Z\b", "<TIME>"),
            (r"\b20\d{6}\.db\b", "<DAY>.db"),
            (r#""id":\d+"#, r#""id":<N>"#),
            (r"\bmcp:[0-9a-f]{32}:", "mcp:<NONCE>:"),
            // A run's report.json bytes hold its run id (the UTC second).
            (r#"("report_sha256": ?")[0-9a-f]{64}""#, r#"$1<SHA256>""#),
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
    local.setup(case, &mock.base)?;
    bridged.setup(case, &mock.base)?;

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
    let only = std::env::var("TENGU_CONFORMANCE_ONLY").ok();
    let queue: Vec<Case> = cases()
        .into_iter()
        .filter(|c| !c.gated || catalog.contains(&c.tool))
        .filter(|c| only.as_deref().is_none_or(|o| c.name.contains(o)))
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

/// Two Claude CLI sessions = two bridge processes that number their
/// requests alike: `paper_order` with JSON-RPC id 3 in both lands two
/// orders (the call-id nonce), never a replay of the first.
#[test]
fn two_bridge_sessions_place_two_orders() {
    let case = paper(case("paper_order", paper_buy()));
    let mock = Mock::start(case.routes.clone());
    let side = Side::new(&case, &mock.base);
    let first = run_bridge(&side, &case, &mock.base).expect("first bridge");
    let second = run_bridge(&side, &case, &mock.base).expect("second bridge");
    for (i, a) in [&first, &second].into_iter().enumerate() {
        let text = &a.last().unwrap().text;
        assert!(
            text.starts_with("paper_fill filled buy hyperliquid:xyz:TSLA")
                && text.contains("replayed=false"),
            "bridge {i}: {text}"
        );
    }
    let ledger = side.root.join("home/.tengu/state/conf/ledger.db");
    let conn = rusqlite::Connection::open(&ledger).unwrap();
    let ids: Vec<String> = conn
        .prepare("SELECT client_order_id FROM orders ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let shape = regex::Regex::new(r"^mcp:[0-9a-f]{32}:3$").unwrap();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids.iter().all(|i| shape.is_match(i)), "{ids:?}");
    assert_ne!(ids[0], ids[1], "one JSON-RPC id, two sessions, two keys");
}
