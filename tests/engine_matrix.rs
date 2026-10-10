//! Engine matrix (`x-engine-matrix-smoke`, `x-engine-parity-audit`,
//! milestone E0, tracker convention 20): one scripted turn per engine × model
//! × tool set. Every catalog tool, one shell skill and one `[[mcp_servers]]`
//! proxy sit in a set (`every_catalog_tool_has_a_live_leg`). Most sets run
//! through `tengu run-agent` on the routable agent of each engine × model
//! (the IPC `compose` hands each leg exactly its set, as a planner step with
//! `Step.compose` does); the xm set holds exec tools, so it runs through
//! `tengu tool turn` on that engine × model's private `xm_*` agent (no
//! `description` — run-agent never runs one).
//!
//! | Fixture family | Files | Sandbox | Sets |
//! |---|---|---|---|
//! | hardened | `tests/fixtures/engine_matrix/{openrouter,claude_code,local}.toml` | `[risk]` + `[xmarket]` + `[paper]` + `[sources]` (one enabled `ted_search` row, synthetic terms; its state dir `engine-matrix` is also the SOE state root, no `[soe]`): no shell, no `[[mcp_servers]]`, Privy signing off | `workspace`, `hyperliquid`, `xm`, `xlab`, `xlab_holdout`, `xlab_rank` (a copy at `<tmp>/sandboxes/rank-test/config.toml` + `[strategy_ranking]`: a ranking contract names its sandbox), `sources`, `soe` |
//! | open | `tests/fixtures/engine_matrix/open/{openrouter,claude_code,local,codex}.toml` | `[memory]` on, a shell, the `matrix` `[[mcp_servers]]` (`token_mcp_server.sh`), no signer | every other set |
//!
//! | Tool set | Scripted calls | The leg also asserts |
//! |---|---|---|
//! | `workspace` | `list_directory` `.` → `read_file` the `token-*` file → `write_file` `answer.txt` = the token → `read_file` `second.txt` | the answer holds the token (its file name is only in the listing, its value only in the file); `answer.txt` = the token; `second.txt` holds a registered secret (`TENGU_SECRETS_LOADED`): the answer quotes `REDACTED`; Claude Code: the bridge's result for it, logged by the engine, is `[REDACTED]` |
//! | `hyperliquid` | `hl_ctx` `{"coins": ["xyz:TSLA"]}` → `hl_book` `{"coin": "xyz:TSLA"}` — live, read-only | the answer holds a number of the stored `mkt_ctx/1:hyperliquid:xyz:TSLA` headline and one of `hl_book/1:hyperliquid:xyz:TSLA` |
//! | `xm` | `hl_ctx` `xyz:TSLA` (live) → `paper_order` $15 market buy naming a seeded opportunity row → `paper_positions` → `paper_close` → a second $15 buy with `exit_at_ms` in the past → `xm_exits` → `risk_status` → `xm_weekend_fade` (one step of rule W on `xyz:TSLA`: `waiting` outside the weekend), on a new paper account (`[xmarket]` + `[risk]` + `[paper]` + `[xmarket.weekend_fade]` + the recorder, ledger in a temp `TENGU_HOME`) | the answer quotes the first buy's `avg_px`; `ledger.db` (under that `TENGU_HOME`) holds the filled buys, the filled `paper_close` sell and the filled `xm_exits` sell under `exit:matrix:hyperliquid:xyz:TSLA:deadline:<opened_ms>`, every order with its call id (`chat:` on this chat path), no open position, and the fade's `matrix-shadow` account; `logs/risk.jsonl` has the exit's verdict (tool `xm_exits`) |
//! | `xlab` | `market_history` `xyz:TSLA` 1h over 2026-09-25T20:00Z … 2026-09-28T15:00Z → `backtest` the fixtures' library strategy `matrix_fade` (rule W on `xyz:TSLA`) → `backtest` an inline spec (`matrix_move`, a move trigger) over the same window — no network: the leg's warehouse (`<TENGU_HOME>/state/engine-matrix/market.db`) is seeded first by `tengu history import-json` from `tests/fixtures/xlab/dataset_xyz_TSLA_1h.json` (67 captured HL bars + 68 funding rows) | the answer quotes the last close (360.2), `ret_bps` (−331.2, within 0.05) and each run's research mean net bps (+76.43, −27.39, within 0.005); both run dirs under `<TENGU_HOME>/state/engine-matrix/backtests/` |
//! | `xlab_holdout` | `backtest` `{"run_id": "20261001T182112Z-conf_rows", "view": "trades", "holdout": true}` (a stored split run's rows by id, its holdout shown: a read; first, before any other run id is in the conversation) → `matrix_move` with `split = time:2026-09-27` (the holdout hidden: the in-sample run) → the same + `holdout = true` (both halves: a second read) — the xlab warehouse + the stored run from `tests/fixtures/xlab/run_conf_rows/` seeded first; a set of its own: weak models misread five results in one turn | the answer quotes the hidden run's mean (+2.07), the holdout half's mean (−56.86) and the worst trade's gross bps (−52.88, only in the `trades` rows) within 0.005; `holdout-reads.jsonl` holds both scripted reads (`backtest` · `matrix_move`, `rows` · `conf_rows`; any order — a repeated or extra read of the split is one more line) and no read of another split |
//! | `xlab_rank` | `strategy_ranking` `{"action": "run", "date": "2026-09-28"}` → `{"action": "latest"}` under the sealed test contract `rank.test.v1` (`tests/fixtures/strategy_ranking/lineage`: `rank_fade` / `rank_follow`, the fixtures' move triggers, cutoff 15:00 UTC) — the xlab warehouse seeded first; a set of its own | the answer names `rank_fade` (weakest) before `rank_follow` (strongest) and quotes both research means (−27.39, +19.79, within 0.005); the date's manifest COMPLETE, both run dirs and `latest.json` under `<TENGU_HOME>/state/engine-matrix/`; no `holdout-reads.jsonl` |
//! | `sources` | `source_evidence` `{"at": "2026-09-30", "mode": "knowable", "source": "ted_search"}` → the same at `2026-10-03` — no network: the leg's `<TENGU_HOME>/state/engine-matrix/sources.db` is seeded first by `tengu sources import` of `tests/fixtures/ted/search_change_notice.json` (notice 657981-2026 and its change notice 674231-2026, read 2026-10-02) | the answer quotes the original's publication number (657981-2026) and the superseding record id in full (`ted_search:674231-2026:<content hash>`, read from the leg's `sources.db`) |
//! | `soe` | `soe_view` `{"run": "cycles/2026-W42", "view": "candidates"}` → `soe_propose` the fixture draft (`tests/fixtures/soe/drafts/proposal.json`) into that week → `soe_challenge` the fixture's flat arguments on `replays/fixture.w42` — the SOE state root `<TENGU_HOME>/state/engine-matrix/` seeded first from `tests/fixtures/soe/state/` (a week open in `PROPOSE`, a replay open in `CHALLENGE`; no network) | the answer quotes `2026-W42.p01` and `2026-W42.c01`; one stamped record each in `proposals.jsonl` / `challenges.jsonl`, the step's agent its `provenance.agent`, generation `UNBOUND` |
//! | `shell` | `run_command` `cat shell-token.txt` → the shell skill `matrix_cat` (`tests/fixtures/skills/matrix_cat`, IPC `compose.skills`) on `skill-token.txt` → the `[[mcp_servers]]` proxy `matrix__token` | the answer holds the three tokens (the MCP one only in the server's env: `$TENGU_MATRIX_MCP_VALUE`, resolved by the run-agent child or the step's bridge) |
//! | `memory` | `memory_ingest` → `memory_search` → `persistent_store` `store` `memo.txt` → `persistent_store` `search` | the answer holds `memo.txt`'s token (only in the file); the disk store `<ws>/memory` exists |
//! | `skills` | `view_skill` + `skill_resource` on the workspace skill `matrix-doc` → `manage_skill` `create` → `skill_distill` (`from_message_index` 1) → `apply_improver_proposal` on `matrix-doc` | the answer holds the doc token and the resource token; the two new skills sit under `<ws>/.tengu/skills/`, the distilled one's `evals/prompts.yaml` holds a fixture from the goal (the step's conversation; Claude Code: the engine's transcript through the bridge); `matrix-doc` holds the improved body |
//! | `util` | `shared_cache` `put` + `get` → `http_request` GET a loopback endpoint → `abi_encode` → `hex_to_uint256` | the answer holds the endpoint's token and the decimal of a random hex; the endpoint was hit |
//! | `privy_off` | `sign_message`, `sign_and_send_transaction` — scopes without wallets (Privy signing off) | both calls refused (`ok = false`) before any env read or request; the answer quotes the refusal (`wallet`). Nothing is ever signed |
//! | `privy` | `get_wallet_address` (a read) — needs `PRIVY_APP_ID`, `PRIVY_APP_SECRET`, `PRIVY_WALLET_ID` (env or the repo's `.env`), else skipped | the answer holds the address (`PRIVY_WALLET_ADDRESS` when known); the app secret (registered) nowhere in the child's output |
//! | `solana_read` | `sol_price`, `dlmm_pools`, `dlmm_pool`, `dlmm_positions`, `jup_perps`, `solana_wallet`, `solana_tx` — live, public mainnet RPC + Jupiter lite + Meteora datapi | every tool stored a row; the answer quotes a number of the `sol_price` and the `dlmm_pool` rows (headline or features, within 1e-5: `quotes_near`) |
//! | `solana_decide` | `lp_swap_plan` + `lp_snapshot` → `hedge_decide` → `lp_decide` (all knobs, `commit` off: the decisions store nothing) | reserve-aware swap plan is callable; `lp_snapshot` stored a row; the answer quotes the snapshot's oracle price (`hedge_decide` `price_usd`, `lp_decide` `cycle_price`) |
//! | `solana_write` | `solana_close_token_accounts`, `jupiter_swap`, `dlmm_open_position`, `dlmm_close_position` (a live position of `LP_OWNER`, else the stale fixture one), `jup_perps_order` — `mode = simulate` only: no signer, no wallet grant | every call ran (`ok`), the answer quotes `simulated`. Nothing is ever signed or sent |
//! | `agentic_memory` | `agentic_memory` `capture` → `recall` — needs `--features postgres_memory` + `TENGU_MEMORY_DATABASE_URL`, else skipped | the answer holds the captured token |
//!
//! Every leg: exit 0, `status = ok`, every tool of the set in the `tools`
//! activity and no run of it failed (but the `privy_off` refusals, which
//! must fail); no `compress_and_store` run failed (Claude Code: the step's
//! bridge serves it); the registered secrets are nowhere in the child's
//! stdout or stderr. Configured scopes that exclude the workspace prove the
//! `run-agent` workspace grant — for Claude Code the bridge's
//! (`TENGU_BRIDGE_GRANT_WORKSPACE`, set by the step engine); the xm scopes
//! name the workspace (no grant on the chat path). Children run in the leg's
//! workspace (no repo `.env`, no `TENGU_PLAN.md`), without `HL_API_URL` /
//! `SOLANA_RPC_URL` (mainnet) and with whatever parent Claude Code session
//! env this process has — the engine strips it (`claude_code.rs`).
//!
//! | Engine · model | Agents (hardened · open) | Tests | Needs |
//! |---|---|---|---|
//! | openrouter · `google/gemini-2.5-flash-lite` | `gemini`, `xm_gemini` · `gemini` | `openrouter_gemini_*` | `OPENROUTER_API_KEY` (env or the repo's `.env`) |
//! | openrouter · `anthropic/claude-haiku-4.5` | `haiku`, `xm_haiku` · `haiku` | `openrouter_haiku_*` | same |
//! | claude_code · `claude-haiku-4-5`, built-ins off | `claude`, `xm_claude` · `claude` | `claude_code_*` | `--features claude_code`, `claude` logged in (subscription); `OPENROUTER_API_KEY` for the `memory` set's embeddings |
//! | codex · `gpt-5.5`, `sandbox = "read-only"` | — · `codex` (open sets only: `engine = "codex"` is refused in a hardened sandbox, so every hardened set's `codex_*` leg is absent) | `codex_*` | feature `codex` (default), `codex` logged in with ChatGPT (`codex login`); `OPENROUTER_API_KEY` for the `memory` set's embeddings |
//! | local · `gemma4:latest` | `gemma`, `xm_gemma` · `gemma` | `local_*` | `TENGU_MATRIX_LOCAL_BASE_URL`; unset ⇒ skipped; loopback on macOS ⇒ skipped (local models run on the operator's PC) |
//! | local → a scripted OpenAI-compatible mock | `gemma`, `xm_gemma` · `gemma` | `offline_local_workspace`, `offline_local_xm` (`risk_status` + `paper_positions`; then an open position's row arrives whole — full instrument id, exit deadline — under the 16k cap), `offline_local_shell`, `offline_local_xlab` (`market_history` with 200 points: the text — table cut to 48 rows — arrives whole under the 16k cap; both `backtest` runs' texts whole too), `offline_local_xlab_holdout` (the hidden run, the holdout read and the stored run's rows, each whole), `offline_local_xlab_rank` (the ranking run and `latest`, each whole, rows weakest first), `offline_local_sources` (`source_evidence` before / after the change notice: the original stands, then `superseded … (correction)`; each text whole under the 16k cap, the buyer's name fenced), `offline_local_soe` (the candidates view, the proposal and the challenge, each whole, each record stamped with the step's agent) (no network; not ignored) | nothing |
//! | — | all | `fixtures_load_and_agree`, `every_catalog_tool_has_a_live_leg` (not ignored) | nothing |
//!
//! Live run (sequential; one `engine_matrix |` result line per leg, a
//! skipped leg says why):
//! `cargo test --features claude_code --test engine_matrix -- --ignored --nocapture --test-threads 1`

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/engine_matrix");
/// The shell set's skill, copied into the leg's workspace.
const MATRIX_CAT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/skills/matrix_cat/SKILL.md"
);
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
/// The opportunity row the xm set's `paper_order` names (seeded per leg).
const OPPORTUNITY: &str = "xm_compare/1:hyperliquid:xyz:TSLA:hyperliquid:xyz:TSLA";
/// The xm set's instrument.
const INSTRUMENT: &str = "hyperliquid:xyz:TSLA";
/// The xlab set's warehouse seed (`tengu history import-json`): 67 hourly
/// `xyz:TSLA` bars from 2026-09-25T20:00Z + 68 funding rows, captured from HL.
const XLAB_DATASET: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/xlab/dataset_xyz_TSLA_1h.json"
);
/// The xlab set's window: every seeded bar.
const XLAB_FROM: &str = "2026-09-25T20:00:00Z";
const XLAB_TO: &str = "2026-09-28T15:00:00Z";
/// The seed's first and last close: `last_close=` and `ret_bps=` of the row.
const XLAB_FIRST_CLOSE: f64 = 372.33;
const XLAB_LAST_CLOSE: f64 = 360.2;
/// The hardened fixtures' library strategy (`[backtest.strategies]`): rule W
/// on `xyz:TSLA` — the seeded weekend's one trade.
const XLAB_STRATEGY: &str = "matrix_fade";
/// Research-arm mean net bps over the window (fixed data, fixed costs:
/// `tengu tool call -c <hardened fixture> --tool backtest` prints them):
/// `matrix_fade`, then the inline spec of [`xlab_spec`].
const XLAB_FADE_MEAN_BPS: f64 = 76.43;
const XLAB_MOVE_MEAN_BPS: f64 = -27.39;
/// The xlab_holdout set's split of [`xlab_spec`], between its two trades
/// (decided 2026-09-26T01:00Z, +2.07 net bps · 2026-09-27T23:00Z, −56.86 net
/// bps, gross −52.88 — simple-return P&L; the stored run in the fixture was written by the same engine): the hidden run's in-sample mean and the holdout read's
/// holdout half.
const XLAB_SPLIT: &str = "time:2026-09-27T00:00:00Z";
const XLAB_IN_SAMPLE_MEAN_BPS: f64 = 2.07;
const XLAB_HOLDOUT_MEAN_BPS: f64 = -56.86;
/// A stored run the leg's warehouse is seeded with
/// (`tests/fixtures/xlab/run_conf_rows/`: [`xlab_spec`]'s holdout read, made
/// by the tool on the same dataset) — read by its id with `run_id` + `view`,
/// its holdout rows too (a second counted read): the worst trade's gross
/// bps, printed only by those rows.
const XLAB_STORED_RUN: &str = "20261001T182112Z-conf_rows";
const XLAB_STORED_RUN_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/xlab/run_conf_rows"
);
const XLAB_WORST_GROSS_BPS: f64 = -52.88;
/// The xlab_rank set's registry: the sealed test contract `rank.test.v1`
/// (`rank_fade` / `rank_follow`, cutoff 15:00 UTC, sandbox `rank-test`).
const RANK_REGISTRY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/strategy_ranking/lineage"
);
const RANK_CONTRACT: &str = "rank.test.v1";
/// The sandbox the contract names: the leg's config dir.
const RANK_SANDBOX: &str = "rank-test";
/// The ranked date: its cutoff (15:00 UTC) closes the seeded bars.
const RANK_DATE: &str = "2026-09-28";
/// Research-arm means of the ranked pair over the seeded bars: the fade
/// (rank 1, weakest) is [`xlab_spec`]; the follow is its placebo (rank 2).
const RANK_FADE_MEAN_BPS: f64 = -27.39;
const RANK_FOLLOW_MEAN_BPS: f64 = 19.79;

/// The sources set's seed (`tengu sources import`): the captured TED pair —
/// notice 657981-2026 (published 2026-09-24) and its change notice
/// 674231-2026 (2026-10-01) of one procedure — read as of 2026-10-02.
const SOURCES_SEED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/ted/search_change_notice.json"
);
const SOURCES_OBSERVED_AT: &str = "2026-10-02T08:00:00Z";
/// The two publication numbers, in full.
const SOURCES_ORIGINAL: &str = "657981-2026";
const SOURCES_CHANGE: &str = "674231-2026";
/// The sources set's reads (knowable mode): before the change notice was
/// published, then after it.
const SOURCES_BEFORE: &str = "2026-09-30";
const SOURCES_AFTER: &str = "2026-10-03";

/// The xlab set's inline strategy spec (the Architect's level-2 capability):
/// fade a ≥ 25 bps hourly move on `xyz:TSLA`, out after 3 bars.
fn xlab_spec() -> Value {
    json!({"name": "matrix_move", "kind": "move_trigger", "universe": [INSTRUMENT],
           "interval": "1h", "lookback_bars": 1, "threshold_bps": 25,
           "direction": "fade", "hold_bars": 3})
}
/// The soe set's fixtures and the hardened fixtures' `[sources] state` (the
/// SOE state root under the leg's `TENGU_HOME`).
const SOE_FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/soe");
/// The SOE state root = the hardened fixtures' `[sources] state` (critic
/// C8), shared with `[xmarket]`.
const SOE_STATE: &str = XM_STATE;
/// The seeded runs: a week open in `PROPOSE`, a replay open in `CHALLENGE`.
const SOE_RUN: &str = "cycles/2026-W42";
const SOE_REPLAY: &str = "replays/fixture.w42";
/// What the two writes print: the stamped ids.
const SOE_PROPOSAL_ID: &str = "2026-W42.p01";
const SOE_CHALLENGE_ID: &str = "2026-W42.c01";

/// `xm_exits`' id for the second buy (its `exit_at_ms` is in the past),
/// up to the position's `opened_ms`.
const EXIT_ID_PREFIX: &str = "exit:matrix:hyperliquid:xyz:TSLA:deadline:";

// Solana mainnet ids the Solana sets read (`tests/fixtures/solana/*`), in full.
/// A wallet with SOL + USDC and no perps.
const WALLET: &str = "F3YvPiLdniRPGpeKrbeGWR2zg2wPpzVuvqBA5BBJBQ5S";
/// Owner of positions in `POOL`.
const LP_OWNER: &str = "JBggt27MzM4eohjumT9Tuec7MBoWAgDM4BJjkoisDUcs";
/// The SOL-USDC DLMM pool.
const POOL: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";
/// A closed position of `LP_OWNER` (the close simulates a refusal) — the
/// fallback when no live one is found.
const STALE_POSITION: &str = "H9fmcxgheDvVSn9iUeRSvZPAgTY5WXqvroNpkZ2HCVRW";
const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const TX_OK: &str =
    "L8TEY2sSvscX2R2EBChD1p1o3HApdBUHqVfTJKLxJ4zK4M6pebL3diuYKcKjPF5deW7GF6DVnMdPU2tBwgz1Mfi";
/// `hedge_decide` knobs (all required).
const HEDGE_KNOBS: &str = r#"{"target_delta_sol": 0.0, "delta_threshold_sol": 0.5, "band_bins": 0, "bin_count": 20, "cap_mult": 1.0, "max_notional_usd": 0.0, "min_collateral_ratio": 0.2, "target_collateral_ratio": 0.3, "carry_cap_bps": 0.0, "cooldown_ms": 0, "lp_input": "live", "include_wallet_sol": false, "min_wallet_sol": 0.1, "rent_reserve_sol": 0.05, "max_divergence_bps": 100.0, "max_snapshot_age_secs": 30, "trend_confirm_ms": 0, "no_lp_grace_ms": 0}"#;
/// `lp_decide` knobs (all required).
const LP_KNOBS: &str = r#"{"imbalance_threshold": 0.8, "bin_count": 20, "storm_pct_5m": 0.0, "trend_confirm_ms": 0, "reentry_confirm_ms": 0, "reentry_tol_frac": 0.2, "max_divergence_bps": 100.0, "max_snapshot_age_secs": 30, "min_wallet_sol": 0.1, "rent_reserve_sol": 0.05}"#;

// ---------------------------------------------------------------------------
// Matrix
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    OpenRouter,
    ClaudeCode,
    Local,
    Codex,
}

/// One engine × model: its fixtures, its routable agent (the same name in
/// both families) and its private exec agent (the xm set).
#[derive(Clone, Copy)]
struct Target {
    kind: Kind,
    fixture: &'static str,
    open_fixture: &'static str,
    agent: &'static str,
    xm_agent: &'static str,
    label: &'static str,
}

const GEMINI: Target = Target {
    kind: Kind::OpenRouter,
    fixture: "openrouter.toml",
    open_fixture: "open/openrouter.toml",
    agent: "gemini",
    xm_agent: "xm_gemini",
    label: "openrouter google/gemini-2.5-flash-lite",
};
const HAIKU: Target = Target {
    kind: Kind::OpenRouter,
    fixture: "openrouter.toml",
    open_fixture: "open/openrouter.toml",
    agent: "haiku",
    xm_agent: "xm_haiku",
    label: "openrouter anthropic/claude-haiku-4.5",
};
const CLAUDE: Target = Target {
    kind: Kind::ClaudeCode,
    fixture: "claude_code.toml",
    open_fixture: "open/claude_code.toml",
    agent: "claude",
    xm_agent: "xm_claude",
    label: "claude_code claude-haiku-4-5",
};
const GEMMA: Target = Target {
    kind: Kind::Local,
    fixture: "local.toml",
    open_fixture: "open/local.toml",
    agent: "gemma",
    xm_agent: "xm_gemma",
    label: "local gemma4:latest",
};
/// Open sets only: `engine = "codex"` is refused in the hardened family
/// (`config/hardening.rs`), so `fixture` / `xm_agent` are never used.
const CODEX: Target = Target {
    kind: Kind::Codex,
    fixture: "open/codex.toml",
    open_fixture: "open/codex.toml",
    agent: "codex",
    xm_agent: "codex",
    label: "codex gpt-5.5",
};
/// `GEMMA` against the offline mock server.
const MOCK: Target = Target {
    label: "local mock server",
    ..GEMMA
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Set {
    Workspace,
    Hyperliquid,
    Xm,
    Xlab,
    XlabHoldout,
    XlabRank,
    Sources,
    Soe,
    Shell,
    Memory,
    Skills,
    Util,
    PrivyOff,
    Privy,
    SolanaRead,
    SolanaDecide,
    SolanaWrite,
    AgenticMemory,
}

impl Set {
    const ALL: [Set; 18] = [
        Set::Workspace,
        Set::Hyperliquid,
        Set::Xm,
        Set::Xlab,
        Set::XlabHoldout,
        Set::XlabRank,
        Set::Sources,
        Set::Soe,
        Set::Shell,
        Set::Memory,
        Set::Skills,
        Set::Util,
        Set::PrivyOff,
        Set::Privy,
        Set::SolanaRead,
        Set::SolanaDecide,
        Set::SolanaWrite,
        Set::AgenticMemory,
    ];

    fn name(self) -> &'static str {
        match self {
            Set::Workspace => "workspace",
            Set::Hyperliquid => "hyperliquid",
            Set::Xm => "xm",
            Set::Xlab => "xlab",
            Set::XlabHoldout => "xlab_holdout",
            Set::XlabRank => "xlab_rank",
            Set::Sources => "sources",
            Set::Soe => "soe",
            Set::Shell => "shell",
            Set::Memory => "memory",
            Set::Skills => "skills",
            Set::Util => "util",
            Set::PrivyOff => "privy_off",
            Set::Privy => "privy",
            Set::SolanaRead => "solana_read",
            Set::SolanaDecide => "solana_decide",
            Set::SolanaWrite => "solana_write",
            Set::AgenticMemory => "agentic_memory",
        }
    }

    /// The hardened fixtures (`[risk]`) or the open ones.
    fn hardened(self) -> bool {
        matches!(
            self,
            Set::Workspace
                | Set::Hyperliquid
                | Set::Xm
                | Set::Xlab
                | Set::XlabHoldout
                | Set::XlabRank
                | Set::Sources
                | Set::Soe
        )
    }

    fn fixture(self, target: Target) -> &'static str {
        if self.hardened() {
            target.fixture
        } else {
            target.open_fixture
        }
    }

    /// The tools the leg composes (IPC `compose.tools`, or the private
    /// agent's `tools` for `Xm`): catalog and `[[mcp_servers]]` tools.
    fn tools(self) -> &'static [&'static str] {
        match self {
            Set::Workspace => &["list_directory", "read_file", "write_file"],
            Set::Hyperliquid => &["hl_ctx", "hl_book"],
            Set::Xm => &[
                "hl_ctx",
                "paper_order",
                "paper_positions",
                "paper_close",
                "xm_exits",
                "risk_status",
                "xm_weekend_fade",
            ],
            Set::Xlab => &["market_history", "backtest"],
            Set::XlabHoldout => &["backtest"],
            Set::XlabRank => &["strategy_ranking"],
            Set::Sources => &["source_evidence"],
            Set::Soe => &["soe_view", "soe_propose", "soe_challenge"],
            Set::Shell => &["run_command", "matrix__token"],
            Set::Memory => &["memory_ingest", "memory_search", "persistent_store"],
            Set::Skills => &[
                "view_skill",
                "skill_resource",
                "manage_skill",
                "skill_distill",
                "apply_improver_proposal",
            ],
            Set::Util => &[
                "shared_cache",
                "http_request",
                "abi_encode",
                "hex_to_uint256",
            ],
            Set::PrivyOff => &["sign_message", "sign_and_send_transaction"],
            Set::Privy => &["get_wallet_address"],
            Set::SolanaRead => &[
                "sol_price",
                "dlmm_pools",
                "dlmm_pool",
                "dlmm_positions",
                "jup_perps",
                "solana_wallet",
                "solana_tx",
            ],
            Set::SolanaDecide => &["lp_swap_plan", "lp_snapshot", "hedge_decide", "lp_decide"],
            Set::SolanaWrite => &[
                "solana_close_token_accounts",
                "jupiter_swap",
                "dlmm_open_position",
                "dlmm_close_position",
                "jup_perps_order",
            ],
            Set::AgenticMemory => &["agentic_memory"],
        }
    }

    /// Shell skills the leg loads (IPC `compose.skills`).
    fn skills(self) -> &'static [&'static str] {
        match self {
            Set::Shell => &["matrix_cat"],
            _ => &[],
        }
    }

    /// Every tool whose runs the leg checks: `tools` + `skills`.
    fn expected(self) -> Vec<&'static str> {
        self.tools().iter().chain(self.skills()).copied().collect()
    }

    /// Tools whose every run must be a refusal (`ok = false`).
    fn refused(self) -> &'static [&'static str] {
        match self {
            Set::PrivyOff => &["sign_message", "sign_and_send_transaction"],
            _ => &[],
        }
    }

    /// Env the set needs beyond the engine's own; `Err` = why the leg is
    /// skipped (credentials this Mac lacks).
    fn needs(self) -> Result<Vec<(&'static str, String)>, String> {
        match self {
            Set::Memory => {
                let key = env_or_dotenv("OPENROUTER_API_KEY")
                    .ok_or("needs OPENROUTER_API_KEY (embeddings)")?;
                Ok(vec![("OPENROUTER_API_KEY", key)])
            }
            Set::Privy => {
                let mut envs = Vec::new();
                for var in ["PRIVY_APP_ID", "PRIVY_APP_SECRET", "PRIVY_WALLET_ID"] {
                    let v = env_or_dotenv(var).ok_or_else(|| format!("needs {var}"))?;
                    envs.push((var, v));
                }
                // The app secret is registered like a vault value.
                envs.push((
                    "TENGU_SECRETS_LOADED",
                    format!("{SECRET_VAR},PRIVY_APP_SECRET"),
                ));
                Ok(envs)
            }
            Set::AgenticMemory => {
                if !cfg!(feature = "postgres_memory") {
                    return Err("needs --features postgres_memory".into());
                }
                let url = std::env::var("TENGU_MEMORY_DATABASE_URL")
                    .ok()
                    .filter(|u| !u.is_empty())
                    .ok_or("needs TENGU_MEMORY_DATABASE_URL (Postgres + pgvector)")?;
                Ok(vec![("TENGU_MEMORY_DATABASE_URL", url)])
            }
            _ => Ok(Vec::new()),
        }
    }

    /// The scripted goal: numbered steps, one tool call each, then a
    /// one-line answer that can only come from the results.
    fn goal(self, ws: &Workspace, prep: &Prep) -> String {
        let call = |tool: &str, args: Value| format!("Call {tool} with {args}.");
        let (steps, answer, note): (Vec<String>, &str, &str) = match self {
            Set::Workspace => (
                vec![
                    "Call list_directory with path \".\". Exactly one file name starts with \"token-\".".into(),
                    "Call read_file on that file. Its whole content is the token.".into(),
                    "Call write_file with path \"answer.txt\" and content exactly the token.".into(),
                    "Call read_file on \"second.txt\".".into(),
                ],
                "the token, then the exact text read_file returned for second.txt",
                "",
            ),
            Set::Hyperliquid => (
                vec![
                    call("hl_ctx", json!({"coins": ["xyz:TSLA"]})),
                    call("hl_book", json!({"coin": "xyz:TSLA"})),
                ],
                "the mark= value hl_ctx returned and the bid= value hl_book returned, exactly as printed",
                "",
            ),
            Set::Xm => {
                let buy = json!({"instrument": INSTRUMENT, "side": "buy", "notional_usd": 15,
                                 "kind": "market", "max_slippage_bps": 30, "opportunity": OPPORTUNITY});
                let mut late = buy.clone();
                late["exit_at_ms"] = json!(1_000_000_000_000_i64);
                (
                    vec![
                        call("hl_ctx", json!({"coins": ["xyz:TSLA"]})),
                        call("paper_order", buy),
                        "Call paper_positions with no arguments.".into(),
                        call(
                            "paper_close",
                            json!({"instrument": INSTRUMENT, "max_slippage_bps": 50}),
                        ),
                        call("paper_order", late),
                        "Call xm_exits with no arguments.".into(),
                        "Call risk_status with no arguments.".into(),
                        "Call xm_weekend_fade with no arguments.".into(),
                    ],
                    "the avg_px= value the first paper_order returned and the equity= value risk_status returned, exactly as printed",
                    "",
                )
            }
            Set::Xlab => (
                vec![
                    call(
                        "market_history",
                        json!({"instrument": INSTRUMENT, "interval": "1h",
                               "from": XLAB_FROM, "to": XLAB_TO}),
                    ),
                    call(
                        "backtest",
                        json!({"strategy": XLAB_STRATEGY, "from": XLAB_FROM, "to": XLAB_TO}),
                    ),
                    call(
                        "backtest",
                        json!({"spec": xlab_spec(), "from": XLAB_FROM, "to": XLAB_TO}),
                    ),
                ],
                "the last_close= value and the ret_bps= value market_history returned, then the mean_net_bps= value each backtest call returned on its first line, exactly as printed",
                "",
            ),
            // The stored run is read first: no other run id is in the
            // conversation yet (a weak model swaps in the newest one).
            Set::XlabHoldout => (
                vec![
                    call(
                        "backtest",
                        json!({"run_id": XLAB_STORED_RUN, "view": "trades", "holdout": true}),
                    ),
                    call(
                        "backtest",
                        json!({"spec": xlab_spec(), "from": XLAB_FROM, "to": XLAB_TO,
                               "split": XLAB_SPLIT}),
                    ),
                    call(
                        "backtest",
                        json!({"spec": xlab_spec(), "from": XLAB_FROM, "to": XLAB_TO,
                               "split": XLAB_SPLIT, "holdout": true}),
                    ),
                ],
                "the gross_bps of the worst trade step 1 listed, then the mean_net_bps= value step 2 returned on its first line, then the mean_net_bps= value step 3 printed on its first line (the HOLDOUT line), exactly as printed",
                "",
            ),
            Set::XlabRank => (
                vec![
                    call(
                        "strategy_ranking",
                        json!({"action": "run", "date": RANK_DATE}),
                    ),
                    call("strategy_ranking", json!({"action": "latest"})),
                ],
                "the name and the mean of the weakest strategy the ranking lists (rank 1), then the name and the mean of the strongest (the last rank), exactly as printed",
                "",
            ),
            Set::Sources => {
                let at = |day: &str| json!({"at": day, "mode": "knowable", "source": "ted_search"});
                (
                    vec![
                        call("source_evidence", at(SOURCES_BEFORE)),
                        call("source_evidence", at(SOURCES_AFTER)),
                    ],
                    "the publication number inside the record id of the one fact step 1 listed (between `ted_search:` and the next `:`), then the full record id that step 2's superseded line names after `by`, exactly as printed",
                    "",
                )
            }
            Set::Soe => (
                vec![
                    call("soe_view", json!({"run": SOE_RUN, "view": "candidates"})),
                    call(
                        "soe_propose",
                        json!({"run": SOE_RUN, "proposal": soe_draft("proposal.json")}),
                    ),
                    call("soe_challenge", soe_challenge_args(SOE_REPLAY)),
                ],
                "the proposal id soe_propose returned, then the challenge id soe_challenge returned, exactly as printed",
                "Pass every argument verbatim, the proposal object whole. ",
            ),
            Set::Shell => (
                vec![
                    call("run_command", json!({"command": "cat shell-token.txt"})),
                    call("matrix_cat", json!({"path": "skill-token.txt"})),
                    "Call matrix__token with no arguments.".into(),
                ],
                "the three tokens those calls returned, in that order, separated by spaces",
                "",
            ),
            Set::Memory => (
                vec![
                    call(
                        "memory_ingest",
                        json!({"content": format!("The engine-matrix memory token is {}.", ws.memory_token)}),
                    ),
                    call("memory_search", json!({"query": "engine-matrix memory token"})),
                    call(
                        "persistent_store",
                        json!({"operation": "store", "file_path": "memo.txt"}),
                    ),
                    call(
                        "persistent_store",
                        json!({"operation": "search", "query": "kiwi crate inventory file token"}),
                    ),
                ],
                "the file token the persistent_store search returned from memo.txt",
                "",
            ),
            Set::Skills => (
                vec![
                    call("view_skill", json!({"action": "read", "skill": "matrix-doc"})),
                    call(
                        "skill_resource",
                        json!({"action": "read", "skill": "matrix-doc", "path": "notes.md"}),
                    ),
                    call(
                        "manage_skill",
                        json!({"action": "create", "name": "matrix-made", "tier": "workspace",
                               "description": "Made by the engine matrix.", "body": "# matrix-made\n\nBody.\n"}),
                    ),
                    call(
                        "skill_distill",
                        json!({"name": "matrix-distilled", "description": "Distilled by the engine matrix.",
                               "body_markdown": "# matrix-distilled\n\nBody.\n", "metrics": [],
                               "from_message_index": 1, "tier": "workspace"}),
                    ),
                    call(
                        "apply_improver_proposal",
                        json!({"skill": "matrix-doc", "body_markdown": "# matrix-doc\n\nImproved by the engine matrix.\n",
                               "rationale": "engine matrix"}),
                    ),
                ],
                "the doc token view_skill returned and the resource token skill_resource returned",
                "",
            ),
            Set::Util => (
                vec![
                    call(
                        "shared_cache",
                        json!({"operation": "put", "namespace": "matrix", "key": "k", "value": ws.cache_token}),
                    ),
                    call(
                        "shared_cache",
                        json!({"operation": "get", "namespace": "matrix", "key": "k"}),
                    ),
                    call(
                        "http_request",
                        json!({"url": format!("{}/matrix", prep.http_url()), "method": "GET", "return_body": true}),
                    ),
                    call(
                        "abi_encode",
                        json!({"function_signature": "transfer(address,uint256)",
                               "args": ["0x000000000000000000000000000000000000dEaD", "123456789"]}),
                    ),
                    call("hex_to_uint256", json!({"hex": prep.hex})),
                ],
                "the token http_request returned and the decimal number hex_to_uint256 returned",
                "",
            ),
            Set::PrivyOff => (
                vec![
                    call("sign_message", json!({"message": "engine matrix"})),
                    call(
                        "sign_and_send_transaction",
                        json!({"to": "0x000000000000000000000000000000000000dEaD", "value": "0"}),
                    ),
                ],
                "the error text each call returned",
                "Both calls are expected to fail: this sandbox turns signing off. ",
            ),
            Set::Privy => (
                vec!["Call get_wallet_address with no arguments.".into()],
                "the address it returned, exactly as printed",
                "",
            ),
            Set::SolanaRead => (
                vec![
                    call("sol_price", json!({"mint": WSOL})),
                    call("dlmm_pools", json!({"query": "SOL-USDC"})),
                    call("dlmm_pool", json!({"pool": POOL})),
                    call("dlmm_positions", json!({"wallet": LP_OWNER, "pool": POOL})),
                    call("jup_perps", json!({"wallet": WALLET})),
                    call("solana_wallet", json!({"wallet": WALLET})),
                    call("solana_tx", json!({"signature": TX_OK})),
                ],
                "the usd= value sol_price returned and the active_price= value dlmm_pool returned, exactly as printed",
                "",
            ),
            Set::SolanaDecide => {
                let knobs = |k: &str| -> Value { serde_json::from_str(k).unwrap() };
                (
                    vec![
                        call(
                            "lp_swap_plan",
                            json!({"wallet_sol": 0.5, "wallet_usdc": 1000,
                                   "target_sol": 1, "target_usdc": 100,
                                   "permanent_minimum_sol": 0.2, "rent_reserve_sol": 0.1,
                                   "current_price": 100, "slippage_buffer_pct": 0.02,
                                   "context": "rebalance"}),
                        ),
                        call("lp_snapshot", json!({"wallet": WALLET, "pool": POOL})),
                        call(
                            "hedge_decide",
                            json!({"wallet": WALLET, "pool": POOL, "knobs": knobs(HEDGE_KNOBS)}),
                        ),
                        call(
                            "lp_decide",
                            json!({"wallet": WALLET, "pool": POOL, "knobs": knobs(LP_KNOBS)}),
                        ),
                    ],
                    "the price_usd= value hedge_decide returned and the cycle_price= value lp_decide returned, exactly as printed",
                    "",
                )
            }
            Set::SolanaWrite => (
                vec![
                    call("solana_close_token_accounts", json!({"wallet": WALLET})),
                    call(
                        "jupiter_swap",
                        json!({"wallet": WALLET, "input_mint": WSOL, "output_mint": USDC,
                               "amount": 0.001, "oracle_gate_bps": 50}),
                    ),
                    call(
                        "dlmm_open_position",
                        json!({"wallet": WALLET, "pool": POOL, "amount_x": 0.01, "amount_y": 0,
                               "bin_count": 20, "strategy": "spot", "max_active_bin_slippage": 1,
                               "min_wallet_sol": 0.1, "max_new_bin_arrays": 0, "max_divergence_bps": 50}),
                    ),
                    call(
                        "dlmm_close_position",
                        json!({"wallet": LP_OWNER, "pool": POOL, "position": prep.position, "arm_reentry": false}),
                    ),
                    call(
                        "jup_perps_order",
                        json!({"wallet": WALLET, "pool": POOL, "side": "short", "action": "increase",
                               "size_usd": 10, "collateral": 5, "slippage_bps": 50, "max_notional_usd": 100}),
                    ),
                ],
                "the status= value each call returned, in order",
                "Every call only simulates (the default; never pass mode). ",
            ),
            Set::AgenticMemory => (
                vec![
                    call(
                        "agentic_memory",
                        json!({"operation": "capture",
                               "content": format!("The engine-matrix agentic token is {}.", ws.memory_token)}),
                    ),
                    call(
                        "agentic_memory",
                        json!({"operation": "recall", "query": "engine-matrix agentic token"}),
                    ),
                ],
                "the agentic token recall returned",
                "",
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
             {note}Then reply with exactly one line: {answer}. If you call compress_and_store, use \
             that line as its summary.",
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
    openrouter_gemini_xlab => GEMINI, Set::Xlab;
    openrouter_gemini_xlab_holdout => GEMINI, Set::XlabHoldout;
    openrouter_gemini_xlab_rank => GEMINI, Set::XlabRank;
    openrouter_gemini_sources => GEMINI, Set::Sources;
    openrouter_gemini_soe => GEMINI, Set::Soe;
    openrouter_gemini_shell => GEMINI, Set::Shell;
    openrouter_gemini_memory => GEMINI, Set::Memory;
    openrouter_gemini_skills => GEMINI, Set::Skills;
    openrouter_gemini_util => GEMINI, Set::Util;
    openrouter_gemini_privy_off => GEMINI, Set::PrivyOff;
    openrouter_gemini_privy => GEMINI, Set::Privy;
    openrouter_gemini_solana_read => GEMINI, Set::SolanaRead;
    openrouter_gemini_solana_decide => GEMINI, Set::SolanaDecide;
    openrouter_gemini_solana_write => GEMINI, Set::SolanaWrite;
    openrouter_gemini_agentic_memory => GEMINI, Set::AgenticMemory;
    openrouter_haiku_workspace => HAIKU, Set::Workspace;
    openrouter_haiku_hyperliquid => HAIKU, Set::Hyperliquid;
    openrouter_haiku_xm => HAIKU, Set::Xm;
    openrouter_haiku_xlab => HAIKU, Set::Xlab;
    openrouter_haiku_xlab_holdout => HAIKU, Set::XlabHoldout;
    openrouter_haiku_xlab_rank => HAIKU, Set::XlabRank;
    openrouter_haiku_sources => HAIKU, Set::Sources;
    openrouter_haiku_soe => HAIKU, Set::Soe;
    openrouter_haiku_shell => HAIKU, Set::Shell;
    openrouter_haiku_memory => HAIKU, Set::Memory;
    openrouter_haiku_skills => HAIKU, Set::Skills;
    openrouter_haiku_util => HAIKU, Set::Util;
    openrouter_haiku_privy_off => HAIKU, Set::PrivyOff;
    openrouter_haiku_privy => HAIKU, Set::Privy;
    openrouter_haiku_solana_read => HAIKU, Set::SolanaRead;
    openrouter_haiku_solana_decide => HAIKU, Set::SolanaDecide;
    openrouter_haiku_solana_write => HAIKU, Set::SolanaWrite;
    openrouter_haiku_agentic_memory => HAIKU, Set::AgenticMemory;
    claude_code_workspace => CLAUDE, Set::Workspace;
    claude_code_hyperliquid => CLAUDE, Set::Hyperliquid;
    claude_code_xm => CLAUDE, Set::Xm;
    claude_code_xlab => CLAUDE, Set::Xlab;
    claude_code_xlab_holdout => CLAUDE, Set::XlabHoldout;
    claude_code_xlab_rank => CLAUDE, Set::XlabRank;
    claude_code_sources => CLAUDE, Set::Sources;
    claude_code_soe => CLAUDE, Set::Soe;
    claude_code_shell => CLAUDE, Set::Shell;
    claude_code_memory => CLAUDE, Set::Memory;
    claude_code_skills => CLAUDE, Set::Skills;
    claude_code_util => CLAUDE, Set::Util;
    claude_code_privy_off => CLAUDE, Set::PrivyOff;
    claude_code_privy => CLAUDE, Set::Privy;
    claude_code_solana_read => CLAUDE, Set::SolanaRead;
    claude_code_solana_decide => CLAUDE, Set::SolanaDecide;
    claude_code_solana_write => CLAUDE, Set::SolanaWrite;
    claude_code_agentic_memory => CLAUDE, Set::AgenticMemory;
    local_workspace => GEMMA, Set::Workspace;
    local_hyperliquid => GEMMA, Set::Hyperliquid;
    local_xm => GEMMA, Set::Xm;
    local_xlab => GEMMA, Set::Xlab;
    local_xlab_holdout => GEMMA, Set::XlabHoldout;
    local_xlab_rank => GEMMA, Set::XlabRank;
    local_sources => GEMMA, Set::Sources;
    local_soe => GEMMA, Set::Soe;
    local_shell => GEMMA, Set::Shell;
    local_memory => GEMMA, Set::Memory;
    local_skills => GEMMA, Set::Skills;
    local_util => GEMMA, Set::Util;
    local_privy_off => GEMMA, Set::PrivyOff;
    local_privy => GEMMA, Set::Privy;
    local_solana_read => GEMMA, Set::SolanaRead;
    local_solana_decide => GEMMA, Set::SolanaDecide;
    local_solana_write => GEMMA, Set::SolanaWrite;
    local_agentic_memory => GEMMA, Set::AgenticMemory;
    codex_shell => CODEX, Set::Shell;
    codex_memory => CODEX, Set::Memory;
    codex_skills => CODEX, Set::Skills;
    codex_util => CODEX, Set::Util;
    codex_privy_off => CODEX, Set::PrivyOff;
    codex_privy => CODEX, Set::Privy;
    codex_solana_read => CODEX, Set::SolanaRead;
    codex_solana_decide => CODEX, Set::SolanaDecide;
    codex_solana_write => CODEX, Set::SolanaWrite;
    codex_agentic_memory => CODEX, Set::AgenticMemory;
}

fn live_leg(target: Target, set: Set) {
    let skip = |why: &str| println!("engine_matrix | {} | {} | {why}", target.label, set.name());
    let (mut envs, timeout) = match target.kind {
        Kind::OpenRouter => {
            let key = env_or_dotenv("OPENROUTER_API_KEY")
                .expect("OPENROUTER_API_KEY is not set (env or the repo's .env)");
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
            Err(why) => return skip(why),
        },
        Kind::Codex => {
            if set.hardened() || set == Set::Xm {
                return skip("refused: engine = \"codex\" is not allowed in a hardened sandbox");
            }
            assert!(
                cfg!(feature = "codex"),
                "build with --features codex (the tengu binary runs the engine)"
            );
            let log = "tengu::adapters::outbound::engines::codex=debug";
            (vec![("RUST_LOG", log.to_string())], 600)
        }
    };
    match set.needs() {
        Ok(extra) => envs.extend(extra),
        Err(why) => return skip(&format!("skipped ({why})")),
    }
    let ws = workspace();
    let prep = prepare(target, set, &ws, &envs);
    let timeout = Duration::from_secs(timeout);
    let leg = match set {
        Set::Xm => run_turn_leg(target, set, &ws, &prep, &envs, timeout),
        _ => run_leg(target, set, &ws, &prep, &envs, timeout),
    };
    assert_leg(target, set, &leg, &ws, &prep);
}

/// `var` from the env, else from the repo's `.env` (read here, handed to
/// the child only; never printed).
fn env_or_dotenv(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .filter(|k| !k.is_empty())
        .or_else(|| {
            dotenvy::from_path_iter(Path::new(env!("CARGO_MANIFEST_DIR")).join(".env"))
                .ok()?
                .flatten()
                .find(|(k, _)| k == var)
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

fn token(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

/// A leg's workspace (token file, decoy, [`SECRET_FILE`]), `TENGU_HOME`, and
/// the tokens only a tool can read (the sets write the files they need).
struct Workspace {
    _dir: tempfile::TempDir,
    path: PathBuf,
    _home: tempfile::TempDir,
    home: PathBuf,
    token_file: String,
    token: String,
    secret: String,
    shell_token: String,
    skill_token: String,
    mcp_token: String,
    memory_token: String,
    memo_token: String,
    doc_token: String,
    resource_token: String,
    cache_token: String,
    http_token: String,
}

fn workspace() -> Workspace {
    let dir = tempfile::tempdir().expect("workspace");
    // Canonical: scope checks compare resolved paths (macOS /var → /private/var).
    let path = std::fs::canonicalize(dir.path()).expect("canonical workspace");
    let home = tempfile::tempdir().expect("TENGU_HOME");
    let main_token = token("matrix");
    let token_file = format!("token-{}.txt", uuid::Uuid::new_v4().simple());
    let secret = token("matrix-second");
    std::fs::write(path.join(&token_file), &main_token).unwrap();
    std::fs::write(path.join("notes.txt"), "not the token\n").unwrap();
    std::fs::write(path.join(SECRET_FILE), &secret).unwrap();
    Workspace {
        _dir: dir,
        path,
        home: home.path().to_path_buf(),
        _home: home,
        token_file,
        token: main_token,
        secret,
        shell_token: token("shell"),
        skill_token: token("skill"),
        mcp_token: token("mcp"),
        memory_token: token("memory"),
        memo_token: token("memo"),
        doc_token: token("doc"),
        resource_token: token("resource"),
        cache_token: token("cache"),
        http_token: token("http"),
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(FIXTURES).join(name)
}

/// The util set's loopback endpoint: `GET <url>/matrix` answers
/// `{"token": <http token>}`; stops when dropped.
struct HttpMock {
    url: String,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Drop for HttpMock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn http_mock(token: &str) -> HttpMock {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (hits, stop) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicBool::new(false)),
    );
    let (h, s) = (Arc::clone(&hits), Arc::clone(&stop));
    let body = json!({"token": token}).to_string();
    std::thread::spawn(move || {
        while !s.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut sock, _)) => {
                    sock.set_nonblocking(false).ok();
                    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    // One read holds a GET's head; an error just answers.
                    let mut head = [0u8; 8_192];
                    let _ = sock.read(&mut head);
                    h.fetch_add(1, Ordering::SeqCst);
                    let _ = write!(
                        sock,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    });
    HttpMock { url, hits, stop }
}

/// What a set's leg needs besides the workspace tokens.
struct Prep {
    /// The util set's endpoint.
    http: Option<HttpMock>,
    /// The util set's hex input and its decimal value.
    hex: String,
    decimal: String,
    /// The solana write set's position of `LP_OWNER` in `POOL`.
    position: String,
    /// The privy set's expected address (`PRIVY_WALLET_ADDRESS`), if known.
    address: Option<String>,
    /// Values that must never appear in the child's stdout or stderr.
    secrets: Vec<String>,
    /// The leg's config when not its fixture: the xlab_rank set's copy at
    /// `<dir>/sandboxes/rank-test/config.toml` ([`rank_config`]).
    config: Option<PathBuf>,
    _config_dir: Option<tempfile::TempDir>,
}

impl Prep {
    fn http_url(&self) -> &str {
        self.http.as_ref().map_or("", |h| h.url.as_str())
    }
}

/// The files, endpoint and live ids `set` needs (module table).
fn prepare(target: Target, set: Set, ws: &Workspace, envs: &[(&str, String)]) -> Prep {
    let n = u64::from_le_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap()) >> 9;
    let mut prep = Prep {
        http: None,
        hex: format!("{n:#x}"),
        decimal: n.to_string(),
        position: STALE_POSITION.to_string(),
        address: None,
        secrets: vec![ws.secret.clone()],
        config: None,
        _config_dir: None,
    };
    let write = |rel: &str, text: &str| {
        let p = ws.path.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    match set {
        Set::Xm => seed_opportunity(ws),
        Set::Xlab => seed_market_history(target, ws, envs),
        Set::XlabHoldout => {
            seed_market_history(target, ws, envs);
            seed_stored_run(ws);
        }
        Set::XlabRank => {
            seed_market_history(target, ws, envs);
            let (dir, config) = rank_config(target);
            prep.config = Some(config);
            prep._config_dir = Some(dir);
        }
        Set::Sources => seed_sources(target, ws, envs),
        Set::Soe => seed_soe_state(ws),
        Set::Shell => {
            write("shell-token.txt", &ws.shell_token);
            write("skill-token.txt", &ws.skill_token);
            write(
                "skills/matrix_cat/SKILL.md",
                &std::fs::read_to_string(MATRIX_CAT).unwrap(),
            );
        }
        // Nothing like the memory_ingest sentence: the search must find the
        // file, not the ingested fact.
        Set::Memory => write(
            "memo.txt",
            &format!(
                "Kiwi crate inventory, warehouse 7: 42 crates. File token: {}.\n",
                ws.memo_token
            ),
        ),
        Set::Skills => {
            write(
                "skills/matrix-doc/SKILL.md",
                &format!(
                    "---\nname: matrix-doc\ndescription: Engine-matrix doc skill.\neditable_by_learner: true\n---\n\n# matrix-doc\n\nThe doc token is {}.\n",
                    ws.doc_token
                ),
            );
            write(
                "skills/matrix-doc/resources/notes.md",
                &format!("The resource token is {}.\n", ws.resource_token),
            );
        }
        Set::Util => prep.http = Some(http_mock(&ws.http_token)),
        Set::Privy => {
            prep.address = env_or_dotenv("PRIVY_WALLET_ADDRESS");
            prep.secrets.extend(
                envs.iter()
                    .filter(|(k, _)| *k == "PRIVY_APP_SECRET")
                    .map(|(_, v)| v.clone()),
            );
        }
        Set::SolanaWrite => {
            if let Some(p) = live_position(target, ws, envs) {
                prep.position = p;
            }
        }
        _ => {}
    }
    prep
}

/// A position `LP_OWNER` holds in `POOL` now (`dlmm_positions` in-process,
/// no model): the close then simulates a real one.
fn live_position(target: Target, ws: &Workspace, envs: &[(&str, String)]) -> Option<String> {
    let config = fixture(target.open_fixture).display().to_string();
    let args = json!({"wallet": LP_OWNER, "pool": POOL}).to_string();
    let out = leg_command(
        target,
        ws,
        envs,
        &[
            "tool",
            "call",
            "-c",
            &config,
            "--agent",
            target.agent,
            "--tool",
            "dlmm_positions",
            "--args",
            &args,
        ],
    )
    .stdin(Stdio::null())
    .output()
    .ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    v["observation"]["data"]["positions"]
        .as_array()?
        .iter()
        .find_map(|p| p["position"].as_str().map(str::to_string))
}

/// What one `tengu run-agent` / `tengu tool turn` leg produced.
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

/// `tengu <args>` in the leg's workspace with the leg's env: temp
/// `TENGU_HOME`, the target's hardened fixture as `TENGU_CONFIG`, the
/// registered secret, the open fixtures' vars, mainnet HL and Solana;
/// `envs` on top. A parent Claude Code session's env stays (the engine
/// strips it).
fn leg_command(target: Target, ws: &Workspace, envs: &[(&str, String)], args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tengu"));
    cmd.args(args)
        .current_dir(&ws.path)
        .env("TENGU_HOME", &ws.home)
        .env("TENGU_CONFIG", fixture(target.fixture))
        .env("TENGU_MATRIX_WORKSPACE", &ws.path)
        .env("TENGU_MATRIX_FIXTURES", FIXTURES)
        .env("TENGU_MATRIX_MCP_VALUE", &ws.mcp_token)
        .env("TENGU_SECRETS_LOADED", SECRET_VAR)
        .env(SECRET_VAR, &ws.secret)
        .env_remove("TENGU_EGRESS")
        .env_remove("TENGU_SESSION_ID")
        .env_remove("TENGU_AGENT_NAME")
        .env_remove("TENGU_AGENT_IPC")
        .env_remove("TENGU_BRIDGE_GRANT_WORKSPACE")
        .env_remove("TENGU_BRIDGE_SUMMARY_FILE")
        .env_remove("HL_API_URL")
        .env_remove("SOLANA_RPC_URL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd
}

/// Run `tengu run-agent` for `target` over `set`, like `SubprocessRunner`
/// does (`TENGU_AGENT_IPC=1`, one JSON on stdin; `compose` = the set), on
/// the set's fixture, with `envs` on top and a watchdog.
fn run_leg(
    target: Target,
    set: Set,
    ws: &Workspace,
    prep: &Prep,
    envs: &[(&str, String)],
    timeout: Duration,
) -> Leg {
    let mut cmd = leg_command(target, ws, envs, &["run-agent"]);
    let config = prep
        .config
        .clone()
        .unwrap_or_else(|| fixture(set.fixture(target)));
    cmd.env("TENGU_AGENT_IPC", "1").env("TENGU_CONFIG", config);
    let input = json!({
        "goal": set.goal(ws, prep),
        "agent_name": target.agent,
        "model": "",
        "max_turns": 14,
        "session_id": format!("engine-matrix-{}", uuid::Uuid::new_v4().simple()),
        "step_id": format!("matrix-{}", set.name()),
        "compose": {"base_agent": target.agent, "skills": set.skills(), "tools": set.tools()},
    });
    drive(cmd, Some(input.to_string()), timeout, target.label)
}

/// Run `tengu tool turn` as `target`'s private exec agent over `set` — the
/// `@<agent>` chat path (no `run-agent`, no workspace grant).
fn run_turn_leg(
    target: Target,
    set: Set,
    ws: &Workspace,
    prep: &Prep,
    envs: &[(&str, String)],
    timeout: Duration,
) -> Leg {
    let config = fixture(set.fixture(target)).display().to_string();
    let goal = set.goal(ws, prep);
    let args = [
        "tool",
        "turn",
        "-c",
        &config,
        "--agent",
        target.xm_agent,
        "--goal",
        &goal,
    ];
    drive(
        leg_command(target, ws, envs, &args),
        None,
        timeout,
        target.label,
    )
}

/// Spawn `cmd`, write `input` to its stdin, wait with a watchdog.
fn drive(mut cmd: Command, input: Option<String>, timeout: Duration, label: &str) -> Leg {
    let started = Instant::now();
    let mut child = cmd.spawn().expect("spawn tengu");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        if let Some(input) = input {
            stdin.write_all(input.as_bytes()).expect("write stdin");
        }
    }
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
            "tengu ({label}) did not exit within {timeout:?}\n--- stderr (tail) ---\n{}",
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

/// The opportunity row the xm set's `paper_order` names (`min_edge`): 25
/// bps after costs, backing a buy (`overreaction`), stamped now, 10 min TTL,
/// in the leg's workspace store.
fn seed_opportunity(ws: &Workspace) {
    let dir = ws.path.join(".tengu");
    std::fs::create_dir_all(&dir).unwrap();
    let conn = rusqlite::Connection::open(dir.join("observations.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS observations (
           key TEXT PRIMARY KEY, schema TEXT NOT NULL, observed_at_ms INTEGER NOT NULL,
           slot INTEGER, ttl_ms INTEGER NOT NULL, status TEXT NOT NULL, body TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS observations_schema ON observations(schema, observed_at_ms);",
    )
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let row = json!({
        "key": OPPORTUNITY, "schema": "xm_compare/1", "tool": "xm_compare",
        "observed_at_ms": now, "ttl_ms": 600_000, "source": "live", "status": "ok",
        "headline": "compare hyperliquid:xyz:TSLA edge_after_costs_bps=25",
        "features": {"edge_after_costs_bps": 25.0, "side": "buy", "strategy": "overreaction"},
        "data": null
    });
    conn.execute(
        "INSERT OR REPLACE INTO observations(key, schema, observed_at_ms, slot, ttl_ms, status, body)
         VALUES (?1, 'xm_compare/1', ?2, NULL, 600000, 'ok', ?3)",
        rusqlite::params![OPPORTUNITY, now, row.to_string()],
    )
    .unwrap();
}

/// The xlab set's warehouse: `tengu history import-json` of [`XLAB_DATASET`]
/// into the leg's `<TENGU_HOME>/state/engine-matrix/market.db` — the CLI an
/// operator runs. Loaded through the openrouter fixture whatever the
/// target: the hardened fixtures share `[xmarket]` (`fixtures_load_and_agree`).
fn seed_market_history(target: Target, ws: &Workspace, envs: &[(&str, String)]) {
    let config = fixture("openrouter.toml").display().to_string();
    let args = [
        "-c",
        &config,
        "history",
        "import-json",
        "--file",
        XLAB_DATASET,
    ];
    let out = leg_command(target, ws, envs, &args)
        .stdin(Stdio::null())
        .output()
        .expect("spawn tengu history import-json");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        out.status.success() && stdout.contains("135 rows written, 0 error(s)"),
        "seeding market.db failed:\n{stdout}\n{stderr}"
    );
}

/// The sources set's store: `tengu sources import` of [`SOURCES_SEED`] as
/// read at [`SOURCES_OBSERVED_AT`] into the leg's
/// `<TENGU_HOME>/state/engine-matrix/sources.db` — the CLI an operator runs.
/// Through the openrouter fixture whatever the target: the hardened
/// fixtures share `[sources]` (`fixtures_load_and_agree`).
fn seed_sources(target: Target, ws: &Workspace, envs: &[(&str, String)]) {
    let config = fixture("openrouter.toml").display().to_string();
    let args = [
        "-c",
        &config,
        "sources",
        "import",
        "--source",
        "ted_search",
        "--file",
        SOURCES_SEED,
        "--observed-at",
        SOURCES_OBSERVED_AT,
    ];
    let out = leg_command(target, ws, envs, &args)
        .stdin(Stdio::null())
        .output()
        .expect("spawn tengu sources import");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        out.status.success() && stored_record_id(ws, SOURCES_CHANGE).is_some(),
        "seeding sources.db failed:\n{stdout}\n{stderr}"
    );
}

/// The stored record id of TED notice `publication` in the leg's
/// `sources.db`, in full.
fn stored_record_id(ws: &Workspace, publication: &str) -> Option<String> {
    let db = ws.home.join("state").join(XM_STATE).join("sources.db");
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    conn.query_row(
        "SELECT record_id FROM records WHERE source_id = 'ted_search' AND native_id = ?1",
        [publication],
        |r| r.get(0),
    )
    .ok()
}

/// The soe set's state root: `tests/fixtures/soe/state/` (a week open in
/// `PROPOSE`, a replay open in `CHALLENGE`; written by
/// `tools::soe::tests::fixture_state_is_current`) copied to
/// `<TENGU_HOME>/state/<SOE_STATE>/`.
fn seed_soe_state(ws: &Workspace) {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let target = to.join(e.file_name());
            if e.path().is_dir() {
                copy(&e.path(), &target);
            } else {
                std::fs::copy(e.path(), &target).unwrap();
            }
        }
    }
    copy(
        &Path::new(SOE_FIXTURES).join("state"),
        &ws.home.join("state").join(SOE_STATE),
    );
}

/// A fixture draft (`tests/fixtures/soe/drafts/`).
fn soe_draft(name: &str) -> Value {
    let path = Path::new(SOE_FIXTURES).join("drafts").join(name);
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

/// `soe_challenge`'s flat fixture arguments on `run`.
fn soe_challenge_args(run: &str) -> Value {
    let mut a = soe_draft("challenge.json");
    a["run"] = run.into();
    a
}

/// The records the soe set's two writes appended: proposals of the week,
/// challenges of the replay (one JSON line each).
fn soe_records(ws: &Workspace) -> (Vec<Value>, Vec<Value>) {
    let root = ws.home.join("state").join(SOE_STATE);
    let lines = |rel: &str| -> Vec<Value> {
        std::fs::read_to_string(root.join(rel))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    (
        lines("cycles/2026-W42/proposals.jsonl"),
        lines("replays/fixture.w42/challenges.jsonl"),
    )
}

/// The stored run the xlab set reads by id ([`XLAB_STORED_RUN`]): its
/// `report.json` and trade files into the leg's
/// `<TENGU_HOME>/state/engine-matrix/backtests/<run id>/`, as the tool wrote
/// them.
fn seed_stored_run(ws: &Workspace) {
    let dir = ws
        .home
        .join("state")
        .join(XM_STATE)
        .join("backtests")
        .join(XLAB_STORED_RUN);
    std::fs::create_dir_all(&dir).unwrap();
    for f in [
        "report.json",
        "trades-research.jsonl",
        "trades-capped.jsonl",
    ] {
        std::fs::copy(Path::new(XLAB_STORED_RUN_FIXTURE).join(f), dir.join(f))
            .unwrap_or_else(|e| panic!("seed {f}: {e}"));
    }
}

/// The xlab_rank set's config: the target's hardened fixture + a
/// `[strategy_ranking]` naming [`RANK_REGISTRY`], at
/// `<tmp>/sandboxes/rank-test/config.toml` — a ranking contract names its
/// sandbox (the config's dir), and the fixtures sit outside `sandboxes/`.
/// Same `[xmarket]` state: the seeded warehouse.
fn rank_config(target: Target) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("config dir");
    let sandbox = dir.path().join("sandboxes").join(RANK_SANDBOX);
    std::fs::create_dir_all(&sandbox).unwrap();
    let text = std::fs::read_to_string(fixture(target.fixture)).unwrap();
    let config = sandbox.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "{text}\n[strategy_ranking]\nregistry = \"{RANK_REGISTRY}\"\ncontracts = [\"{RANK_CONTRACT}\"]\n"
        ),
    )
    .unwrap();
    (dir, config)
}

/// The xlab_rank set: the answer names the weakest strategy before the
/// strongest and quotes both means; the date is published COMPLETE (both run
/// dirs, `latest`), and no holdout was read.
fn assert_rank(label: &str, leg: &Leg, ws: &Workspace, answer: &str) {
    let (weak, strong) = (answer.find("rank_fade"), answer.find("rank_follow"));
    assert!(
        matches!((weak, strong), (Some(w), Some(s)) if w < s),
        "{label}: the answer does not name rank_fade (weakest) before rank_follow (strongest)\n{}",
        leg.context()
    );
    for (what, bps) in [
        ("rank_fade's mean", RANK_FADE_MEAN_BPS),
        ("rank_follow's mean", RANK_FOLLOW_MEAN_BPS),
    ] {
        assert!(
            quotes_within(answer, bps.abs(), 0.0051),
            "{label}: the answer does not quote {what} {bps:+.2}\n{}",
            leg.context()
        );
    }
    let state = ws.home.join("state").join(XM_STATE);
    let dir = state.join("strategy-rankings").join(RANK_CONTRACT);
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(dir.join(RANK_DATE).join("manifest.json")).unwrap_or_default(),
    )
    .unwrap_or(Value::Null);
    assert_eq!(
        manifest["status"], "COMPLETE",
        "{label}: {RANK_DATE} not published COMPLETE: {manifest:#}"
    );
    assert!(dir.join("latest.json").is_file(), "{label}: no latest.json");
    for s in ["rank_fade", "rank_follow"] {
        let run = manifest["strategies"][s]["run"].as_str().unwrap_or("");
        let id = run.rsplit('/').next().unwrap_or("");
        assert!(
            run.starts_with(&format!("run:{XM_STATE}/"))
                && state.join("backtests").join(id).is_dir(),
            "{label}: {s}'s run `{run}` has no run dir"
        );
    }
    assert!(
        !state.join("backtests/holdout-reads.jsonl").exists(),
        "{label}: a ranking read a holdout"
    );
}

/// One order in the leg's ledger.
#[derive(Debug)]
struct LedgerOrder {
    client_order_id: String,
    side: String,
    status: String,
    avg_px: Option<f64>,
    call_id: Option<String>,
}

fn ledger_path(ws: &Workspace) -> PathBuf {
    ws.home.join("state").join(XM_STATE).join("ledger.db")
}

/// Every order in the leg's ledger, in order.
fn ledger_orders(ws: &Workspace) -> Vec<LedgerOrder> {
    let Ok(conn) = rusqlite::Connection::open(ledger_path(ws)) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn
        .prepare("SELECT client_order_id, side, status, avg_px, call_id FROM orders ORDER BY id")
    else {
        return Vec::new();
    };
    stmt.query_map([], |r| {
        Ok(LedgerOrder {
            client_order_id: r.get(0)?,
            side: r.get(1)?,
            status: r.get(2)?,
            avg_px: r.get(3)?,
            call_id: r.get(4)?,
        })
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

/// The leg's ledger accounts, by name.
fn ledger_accounts(ws: &Workspace) -> Vec<String> {
    let Ok(conn) = rusqlite::Connection::open(ledger_path(ws)) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare("SELECT account FROM accounts ORDER BY account") else {
        return Vec::new();
    };
    stmt.query_map([], |r| r.get(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// The quantity the leg's account holds in `xyz:TSLA` (`None`: no row).
fn open_qty(ws: &Workspace) -> Option<f64> {
    let conn = rusqlite::Connection::open(ledger_path(ws)).ok()?;
    conn.query_row(
        "SELECT qty FROM positions WHERE account = 'matrix' AND instrument = ?1",
        [INSTRUMENT],
        |r| r.get(0),
    )
    .ok()
}

/// An open long in `INSTRUMENT` on the leg's account, written straight into
/// the ledger a leg already created (no network: `paper_order` needs a live
/// book); returns its exit deadline (`exit_at_ms`, an hour ahead).
fn seed_open_position(ws: &Workspace) -> i64 {
    let conn = rusqlite::Connection::open(ledger_path(ws)).expect("ledger");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let exit_at_ms = now + 3_600_000;
    conn.execute(
        "INSERT INTO positions(account, instrument, underlying, venue, qty, avg_px,
           realized_pnl_usd, fees_usd, funding_usd, opened_ms, last_funding_hour_ms,
           exit_at_ms, updated_ms)
         VALUES ('matrix', ?1, 'company:tesla', 'hyperliquid', 0.043, 347.2, 0, 0.0015, 0,
           ?2, NULL, ?3, ?2)",
        rusqlite::params![INSTRUMENT, now, exit_at_ms],
    )
    .expect("seed position");
    exit_at_ms
}

/// `<TENGU_HOME>/logs/risk.jsonl`, one verdict per line.
fn risk_lines(ws: &Workspace) -> Vec<Value> {
    std::fs::read_to_string(ws.home.join("logs").join("risk.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Print the leg's result line, then assert what every leg shares and what
/// its tool set reads (module doc).
fn assert_leg(target: Target, set: Set, leg: &Leg, ws: &Workspace, prep: &Prep) {
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
    for tool in set.expected() {
        assert!(
            runs.iter().any(|(n, _)| n == tool),
            "{label}: {tool} was not called; tools: {called:?}\n{}",
            leg.context()
        );
        let refused = set.refused().contains(&tool);
        assert!(
            !runs.iter().any(|(n, ok)| n == tool && *ok == refused),
            "{label}: {tool} {}; tools: {called:?}\n{}",
            if refused {
                "ran — it must be refused"
            } else {
                "returned an error"
            },
            leg.context()
        );
    }
    assert!(
        !runs.iter().any(|(n, ok)| n == "compress_and_store" && !ok),
        "{label}: compress_and_store failed (a step's bridge serves it); tools: {called:?}\n{}",
        leg.context()
    );
    for secret in &prep.secrets {
        for (what, text) in [("stdout", &leg.stdout), ("stderr", &leg.stderr)] {
            assert!(
                !text.contains(secret.as_str()),
                "{label}: a registered secret leaked in clear to {what}"
            );
        }
    }
    let answer = leg.answer();
    let quotes = |what: &str, value: &str| {
        assert!(
            answer.contains(value),
            "{label}: the answer does not hold {what} `{value}` (result not read)\n{}",
            leg.context()
        );
    };
    match set {
        Set::Workspace => {
            let written = std::fs::read_to_string(ws.path.join("answer.txt")).unwrap_or_default();
            assert_eq!(
                written.trim(),
                ws.token,
                "{label}: answer.txt does not hold the token\n{}",
                leg.context()
            );
            quotes("the token", &ws.token);
            let reads = runs.iter().filter(|(n, _)| n == "read_file").count();
            assert!(reads >= 2, "{label}: {SECRET_FILE} not read: {called:?}");
            // Models may drop the brackets; the word only comes from the result.
            quotes("the redacted second.txt", "REDACTED");
            if target.kind == Kind::ClaudeCode {
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
        Set::Xm => assert_xm(&label, leg, ws, &answer),
        Set::Xlab => {
            assert!(
                quotes_any(&answer, &[XLAB_LAST_CLOSE]),
                "{label}: the answer does not quote the last close {XLAB_LAST_CLOSE}\n{}",
                leg.context()
            );
            // Line 1 prints it to 0.1 bps, the features line to 6 digits;
            // a model may drop the sign.
            let ret = (XLAB_LAST_CLOSE / XLAB_FIRST_CLOSE).ln() * 10_000.0;
            assert!(
                quotes_within(&answer, ret.abs(), 0.051),
                "{label}: the answer does not quote ret_bps {ret:.1}\n{}",
                leg.context()
            );
            // Line 1 prints a mean to 0.01 bps, the features line to 6
            // digits: either one is the result read.
            for (run, mean) in [
                (XLAB_STRATEGY, XLAB_FADE_MEAN_BPS),
                ("matrix_move", XLAB_MOVE_MEAN_BPS),
            ] {
                assert!(
                    quotes_within(&answer, mean.abs(), 0.0051),
                    "{label}: the answer does not quote {run}'s mean_net_bps {mean:+.2}\n{}",
                    leg.context()
                );
            }
            let runs: Vec<String> =
                std::fs::read_dir(ws.home.join("state/engine-matrix/backtests"))
                    .map(|d| {
                        d.flatten()
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .collect()
                    })
                    .unwrap_or_default();
            for run in [XLAB_STRATEGY, "matrix_move"] {
                assert!(
                    runs.iter().any(|r| r.ends_with(&format!("Z-{run}"))),
                    "{label}: no run dir of {run}: {runs:?}"
                );
            }
        }
        Set::Soe => {
            quotes("the proposal id", SOE_PROPOSAL_ID);
            quotes("the challenge id", SOE_CHALLENGE_ID);
            // One stamped record each, the step's agent in its provenance
            // (a run-agent child names it: TENGU_AGENT_NAME, or the
            // bridge's TENGU_BRIDGE_AGENT).
            let (proposals, challenges) = soe_records(ws);
            for (what, rows, id) in [
                ("proposals", &proposals, SOE_PROPOSAL_ID),
                ("challenges", &challenges, SOE_CHALLENGE_ID),
            ] {
                assert_eq!(rows.len(), 1, "{label}: {what}: {rows:?}");
                assert_eq!(rows[0]["id"], id, "{label}: {what}");
                assert_eq!(
                    rows[0]["provenance"]["agent"], target.agent,
                    "{label}: {what}"
                );
                assert_eq!(rows[0]["provenance"]["generation"], "UNBOUND", "{label}");
            }
        }
        Set::XlabHoldout => {
            // The hidden run's in-sample mean, the read's holdout half, and
            // the stored run's worst trade — only its `trades` rows, the
            // holdout shown, print a gross bps.
            for (what, bps) in [
                ("the hidden run's in-sample mean", XLAB_IN_SAMPLE_MEAN_BPS),
                ("the holdout half's mean", XLAB_HOLDOUT_MEAN_BPS),
                ("the worst trade's gross_bps", XLAB_WORST_GROSS_BPS),
            ] {
                assert!(
                    quotes_within(&answer, bps.abs(), 0.0051),
                    "{label}: the answer does not quote {what} {bps:+.2}\n{}",
                    leg.context()
                );
            }
            // Both scripted holdout reads are lines of the ledger — matrix_move's
            // run, the stored run's rows — in any order; a model may call both
            // at once, repeat one or read more rows of the split (each one
            // more line), never another split.
            let ledger = std::fs::read_to_string(
                ws.home
                    .join("state/engine-matrix/backtests/holdout-reads.jsonl"),
            )
            .unwrap_or_default();
            let reads: Vec<(String, String, String)> = ledger
                .lines()
                .map(|l| serde_json::from_str::<Value>(l).unwrap())
                .map(|r| {
                    let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
                    (s("via"), s("strategy"), s("split"))
                })
                .collect();
            for (via, strategy) in [("backtest", "matrix_move"), ("rows", "conf_rows")] {
                let want = (
                    via.to_string(),
                    strategy.to_string(),
                    XLAB_SPLIT.to_string(),
                );
                assert!(
                    reads.contains(&want),
                    "{label}: no holdout read {want:?} in {reads:?}\n{}",
                    leg.context()
                );
            }
            assert!(
                reads.iter().all(|(_, _, split)| split == XLAB_SPLIT),
                "{label}: holdout reads of another split: {reads:?}"
            );
        }
        Set::XlabRank => assert_rank(&label, leg, ws, &answer),
        Set::Sources => {
            // The original notice stood before its change; after it, the
            // change supersedes it — its record id is only in step 2's text.
            quotes("the original notice's publication number", SOURCES_ORIGINAL);
            let change = stored_record_id(ws, SOURCES_CHANGE)
                .unwrap_or_else(|| panic!("{label}: {SOURCES_CHANGE} is not stored"));
            quotes("the superseding record id", &change);
        }
        Set::Shell => {
            quotes("the run_command token", &ws.shell_token);
            quotes("the matrix_cat (shell skill) token", &ws.skill_token);
            quotes("the matrix__token (MCP) token", &ws.mcp_token);
        }
        Set::Memory => {
            quotes("memo.txt's token", &ws.memo_token);
            assert!(
                ws.path.join("memory").exists(),
                "{label}: no disk memory store under the workspace"
            );
        }
        Set::Skills => {
            quotes("the doc token", &ws.doc_token);
            quotes("the resource token", &ws.resource_token);
            for made in ["matrix-made", "matrix-distilled"] {
                let file = ws.path.join(".tengu/skills").join(made).join("SKILL.md");
                assert!(file.is_file(), "{label}: {} missing", file.display());
            }
            // Fixtures from the step's conversation (message 1 on: the
            // goal) — the run-agent loop's messages, or the Claude Code
            // engine's transcript through the bridge.
            let prompts = std::fs::read_to_string(
                ws.path
                    .join(".tengu/skills/matrix-distilled/evals/prompts.yaml"),
            )
            .unwrap_or_default();
            assert!(
                prompts.contains("id: f1") && prompts.contains("Scripted tool test"),
                "{label}: skill_distill seeded no fixture from the conversation:\n{prompts}"
            );
            let doc = std::fs::read_to_string(ws.path.join("skills/matrix-doc/SKILL.md"))
                .unwrap_or_default();
            assert!(
                doc.contains("Improved by the engine matrix"),
                "{label}: apply_improver_proposal did not rewrite matrix-doc:\n{doc}"
            );
        }
        Set::Util => {
            quotes("the endpoint's token", &ws.http_token);
            quotes("the decimal of the hex", &prep.decimal);
            let hits = prep
                .http
                .as_ref()
                .map_or(0, |h| h.hits.load(Ordering::SeqCst));
            assert!(hits >= 1, "{label}: the loopback endpoint was never hit");
        }
        Set::PrivyOff => quotes("the refusal", "wallet"),
        Set::Privy => match &prep.address {
            Some(addr) => assert!(
                answer.to_lowercase().contains(&addr.to_lowercase()),
                "{label}: the answer does not hold the wallet address {addr}\n{}",
                leg.context()
            ),
            None => assert!(
                regex::Regex::new(r"0x[0-9a-fA-F]{40}")
                    .unwrap()
                    .is_match(&answer),
                "{label}: the answer holds no EVM address\n{}",
                leg.context()
            ),
        },
        Set::SolanaRead => {
            for tool in set.tools() {
                assert!(
                    !stored_rows(&ws.path, tool).is_empty(),
                    "{label}: {tool} stored no row\n{}",
                    leg.context()
                );
            }
            // `active_price` is in dlmm_pool's and dlmm_positions' results
            // (two reads seconds apart): either one is a result read.
            for tools in [&["sol_price"][..], &["dlmm_pool", "dlmm_positions"]] {
                let rows: Vec<Value> = tools
                    .iter()
                    .flat_map(|t| stored_rows(&ws.path, t))
                    .collect();
                let numbers = row_numbers(&rows);
                assert!(
                    quotes_near(&answer, &numbers),
                    "{label}: the answer quotes no number of {tools:?}' rows {numbers:?}\n{}",
                    leg.context()
                );
            }
        }
        Set::SolanaDecide => {
            // The decide tools store nothing without `commit = true`; their
            // prices are the snapshot's oracle (rows of lp_snapshot / the
            // inline price fetch).
            assert!(
                !stored_rows(&ws.path, "lp_snapshot").is_empty(),
                "{label}: lp_snapshot stored no row\n{}",
                leg.context()
            );
            let numbers = row_numbers(&all_rows(&ws.path));
            assert!(
                quotes_near(&answer, &numbers),
                "{label}: the answer quotes no price of the snapshot rows {numbers:?}\n{}",
                leg.context()
            );
        }
        Set::SolanaWrite => quotes("a simulate status", "simulated"),
        Set::AgenticMemory => quotes("the captured token", &ws.memory_token),
    }
}

/// The xm set's ledger checks (module table).
fn assert_xm(label: &str, leg: &Leg, ws: &Workspace, answer: &str) {
    let orders = ledger_orders(ws);
    let filled = |side: &str, exit: bool| {
        orders
            .iter()
            .filter(|o| {
                o.side == side
                    && o.status == "filled"
                    && o.client_order_id.starts_with(EXIT_ID_PREFIX) == exit
            })
            .count()
    };
    assert!(
        filled("buy", false) >= 2 && filled("sell", false) >= 1 && filled("sell", true) >= 1,
        "{label}: the ledger lacks the two filled buys, the paper_close sell or the \
         xm_exits sell: {orders:?}\n{}",
        leg.context()
    );
    assert!(
        orders.iter().all(|o| o.call_id.is_some()),
        "{label}: an order without a call id: {orders:?}"
    );
    // Orders keyed on their call id carry the chat path's id
    // (`chat/tool_loop.rs`) or the bridge's — never a provider's own.
    let keyed: Vec<&LedgerOrder> = orders
        .iter()
        .filter(|o| o.call_id.as_deref() == Some(o.client_order_id.as_str()))
        .collect();
    assert!(
        keyed.len() >= 3
            && keyed.iter().all(|o| {
                o.client_order_id.starts_with("chat:") || o.client_order_id.starts_with("mcp:")
            }),
        "{label}: the paper_order / paper_close orders are not keyed on chat: / mcp: call ids: {orders:?}"
    );
    assert_eq!(
        open_qty(ws),
        Some(0.0),
        "{label}: a position is still open: {orders:?}"
    );
    let exits = risk_lines(ws)
        .into_iter()
        .filter(|l| l["tool"] == "xm_exits" && l["verdict"] == "allow")
        .count();
    assert!(exits >= 1, "{label}: no xm_exits verdict in risk.jsonl");
    // xm_weekend_fade ran a step of its window: both ledger accounts exist
    // whatever the phase.
    assert!(
        ledger_accounts(ws).contains(&"matrix-shadow".to_string()),
        "{label}: xm_weekend_fade did not open its shadow account: {:?}",
        ledger_accounts(ws)
    );
    let px = orders
        .iter()
        .find(|o| o.side == "buy" && o.status == "filled")
        .and_then(|o| o.avg_px)
        .expect("a filled order has avg_px");
    assert!(
        quotes_any(answer, &[px]),
        "{label}: the answer does not quote the buy's avg_px {px}\n{}",
        leg.context()
    );
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

/// Bodies of every row in the workspace observation store.
fn all_rows(workspace: &Path) -> Vec<Value> {
    let Ok(conn) = rusqlite::Connection::open(workspace.join(".tengu/observations.db")) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare("SELECT body FROM observations") else {
        return Vec::new();
    };
    stmt.query_map([], |r| r.get::<_, String>(0))
        .map(|rows| {
            rows.flatten()
                .filter_map(|b| serde_json::from_str::<Value>(&b).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Bodies of the rows `tool` left in the workspace observation store.
fn stored_rows(workspace: &Path, tool: &str) -> Vec<Value> {
    all_rows(workspace)
        .into_iter()
        .filter(|v| v["tool"] == tool)
        .collect()
}

/// The ≥ 3-significant-digit numbers of `rows`' headlines and features —
/// what a model quotes from a typed result (line 1, the features line, or
/// `data` at full precision: `quotes_near`).
fn row_numbers(rows: &[Value]) -> Vec<f64> {
    // Prices and amounts, not knobs or counts a goal names (0.5, 20).
    let significant = |n: f64| n.abs() >= 10.0 && (n.fract() != 0.0 || n.abs() >= 1000.0);
    let mut out: Vec<f64> = rows
        .iter()
        .filter_map(|r| r["headline"].as_str())
        .flat_map(headline_numbers)
        .collect();
    out.extend(
        rows.iter()
            .filter_map(|r| r["features"].as_object())
            .flat_map(|f| f.values().filter_map(Value::as_f64))
            .filter(|n| *n != 0.0 && significant(*n)),
    );
    out
}

/// `text` holds one of `numbers` within a relative 1e-5 — a rounded
/// headline value (`118.2268`) and the full-precision one a model may copy
/// from `data` (`118.226775296146`) both count; still ≥ 5 significant
/// digits a model cannot guess.
fn quotes_near(text: &str, numbers: &[f64]) -> bool {
    let re = regex::Regex::new(r"\d+(?:\.\d+)?").unwrap();
    let quoted: Vec<f64> = re
        .find_iter(text)
        .filter_map(|m| m.as_str().parse().ok())
        .collect();
    numbers.iter().any(|n| {
        quoted
            .iter()
            .any(|q| (q - n).abs() <= 1e-5 * n.abs().max(1.0))
    })
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

/// `text` holds a number within `tol` of `value` (unsigned: the digits).
fn quotes_within(text: &str, value: f64, tol: f64) -> bool {
    let re = regex::Regex::new(r"\d+(?:\.\d+)?").unwrap();
    let quoted: Vec<f64> = re
        .find_iter(text)
        .filter_map(|m| m.as_str().parse().ok())
        .collect();
    quoted.iter().any(|q| (q - value).abs() <= tol)
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

fn offline_leg(set: Set, ws: &Workspace, prep: &Prep, replies: Vec<String>) -> (Leg, Vec<String>) {
    let (url, server) = mock_server(replies);
    let envs = [("TENGU_MATRIX_LOCAL_BASE_URL", url)];
    let timeout = Duration::from_secs(25);
    let leg = match set {
        Set::Xm => run_turn_leg(MOCK, set, ws, prep, &envs, timeout),
        _ => run_leg(MOCK, set, ws, prep, &envs, timeout),
    };
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
    let prep = prepare(MOCK, Set::Workspace, &ws, &[]);
    let (leg, bodies) = offline_leg(
        Set::Workspace,
        &ws,
        &prep,
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
    assert_leg(MOCK, Set::Workspace, &leg, &ws, &prep);
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

/// The xm set's network-free tools on the local engine through `tengu tool
/// turn`, scripted: `[xmarket]` + `[risk]` + `[paper]` load for the private
/// `xm_gemma` agent, `risk_status` opens a new account in the leg's
/// `TENGU_HOME`, `paper_positions` reads it; both rows reach the model.
/// Then, with an open position in that ledger, a second turn's
/// `paper_positions` row (under the 16k agent's 8 192-char cap) reaches the
/// model whole: the full instrument id and exit deadline a `paper_close`
/// needs, not a store pointer.
#[test]
fn offline_local_xm() {
    let ws = workspace();
    let prep = prepare(MOCK, Set::Xm, &ws, &[]);
    let (leg, bodies) = offline_leg(
        Set::Xm,
        &ws,
        &prep,
        vec![
            tool_call_reply("c1", "risk_status", &json!({})),
            tool_call_reply("c2", "paper_positions", &json!({})),
            text_reply(&format!("matrix equity={EQUITY}")),
        ],
    );
    assert_eq!(
        (leg.code, leg.ipc["status"].as_str()),
        (Some(0), Some("ok")),
        "{}",
        leg.context()
    );
    let ran: Vec<(String, bool)> = ["risk_status", "paper_positions"]
        .iter()
        .map(|t| (t.to_string(), true))
        .collect();
    assert_eq!(leg.runs(), ran, "{}", leg.context());
    assert_eq!(bodies.len(), 3, "{}", leg.context());
    let mut names = advertised(&bodies[0]);
    names.sort();
    assert_eq!(
        names,
        [
            "hl_ctx",
            "paper_close",
            "paper_order",
            "paper_positions",
            "risk_status",
            "xm_exits",
            "xm_weekend_fade"
        ],
        "the xm agent's tools, no compress_and_store on the chat path"
    );
    let results = tool_messages(&bodies[2]);
    assert!(
        results[0].contains(&format!("risk account=matrix halt=none equity={EQUITY}")),
        "{results:?}"
    );
    assert!(
        results[1].contains("paper_positions account=matrix open=0"),
        "{results:?}"
    );
    let ledger = ws.home.join("state").join(XM_STATE).join("ledger.db");
    assert!(ledger.exists(), "no {}", ledger.display());

    let exit_at_ms = seed_open_position(&ws);
    let (leg, bodies) = offline_leg(
        Set::Xm,
        &ws,
        &prep,
        vec![
            tool_call_reply("c1", "paper_positions", &json!({})),
            text_reply(&format!("matrix open position {INSTRUMENT}")),
        ],
    );
    assert_eq!(
        (leg.code, leg.ipc["status"].as_str()),
        (Some(0), Some("ok")),
        "{}",
        leg.context()
    );
    assert_eq!(
        leg.runs(),
        [("paper_positions".to_string(), true)],
        "{}",
        leg.context()
    );
    assert_eq!(bodies.len(), 2, "{}", leg.context());
    let row = &tool_messages(&bodies[1])[0];
    assert!(
        row.contains("paper_positions account=matrix open=1"),
        "{row}"
    );
    assert!(
        row.contains(&format!("\"instrument\":\"{INSTRUMENT}\"")),
        "the model must see the position's full instrument id: {row}"
    );
    assert!(
        row.contains(&format!("\"exit_at_ms\":{exit_at_ms}")),
        "{row}"
    );
    assert!(!row.contains("bytes in observation"), "{row}");
}

/// The shell set on the local engine, scripted, through `run-agent` on the
/// open fixture: `run_command` (configured scope + the run-agent grant), the
/// shell skill from IPC `compose.skills`, and the `[[mcp_servers]]` proxy
/// (its `$VAR` env resolved in the child) — each result reaches the model,
/// and the model sees only the composed set.
#[test]
fn offline_local_shell() {
    let ws = workspace();
    let prep = prepare(MOCK, Set::Shell, &ws, &[]);
    let (leg, bodies) = offline_leg(
        Set::Shell,
        &ws,
        &prep,
        vec![
            tool_call_reply(
                "c1",
                "run_command",
                &json!({"command": "cat shell-token.txt"}),
            ),
            tool_call_reply("c2", "matrix_cat", &json!({"path": "skill-token.txt"})),
            tool_call_reply("c3", "matrix__token", &json!({})),
            text_reply(&format!(
                "{} {} {}",
                ws.shell_token, ws.skill_token, ws.mcp_token
            )),
        ],
    );
    assert_leg(MOCK, Set::Shell, &leg, &ws, &prep);
    assert_eq!(bodies.len(), 4, "{}", leg.context());
    let mut names = advertised(&bodies[0]);
    names.sort();
    assert_eq!(
        names,
        [
            "compress_and_store",
            "matrix__token",
            "matrix_cat",
            "run_command"
        ]
    );
    let results = tool_messages(&bodies[3]);
    for (i, want) in [&ws.shell_token, &ws.skill_token, &ws.mcp_token]
        .into_iter()
        .enumerate()
    {
        assert!(results[i].contains(want.as_str()), "{results:?}");
    }
}

/// The xlab set on the local engine, scripted, through `run-agent` on the
/// hardened fixture: `market_history` reads the seeded warehouse (configured
/// scope + the run-agent grant, no network) with the most points a call may
/// ask; its text — the table cut to 48 rows, the bars in `data` — reaches
/// the model whole under the 16k agent's 8 192-char cap, line 1 with the
/// full instrument id, and the model sees only the composed set. Then
/// `backtest` runs the library strategy and the inline spec on the same
/// bars: each text whole too — line 1 with the full run id, the rows hint by
/// run id, never the run dir's path.
#[test]
fn offline_local_xlab() {
    let ws = workspace();
    let prep = prepare(MOCK, Set::Xlab, &ws, &[]);
    let (leg, bodies) = offline_leg(
        Set::Xlab,
        &ws,
        &prep,
        vec![
            tool_call_reply(
                "c1",
                "market_history",
                &json!({"instrument": INSTRUMENT, "interval": "1h", "from": XLAB_FROM,
                        "to": XLAB_TO, "points": 200}),
            ),
            tool_call_reply(
                "c2",
                "backtest",
                &json!({"strategy": XLAB_STRATEGY, "from": XLAB_FROM, "to": XLAB_TO}),
            ),
            tool_call_reply(
                "c3",
                "backtest",
                &json!({"spec": xlab_spec(), "from": XLAB_FROM, "to": XLAB_TO}),
            ),
            text_reply(&format!(
                "last_close=360.2 ret_bps=-331.2 mean_net_bps={XLAB_FADE_MEAN_BPS:+.2} \
                 mean_net_bps={XLAB_MOVE_MEAN_BPS:+.2}"
            )),
        ],
    );
    assert_leg(MOCK, Set::Xlab, &leg, &ws, &prep);
    assert_eq!(bodies.len(), 4, "{}", leg.context());
    let mut names = advertised(&bodies[0]);
    names.sort();
    assert_eq!(names, ["backtest", "compress_and_store", "market_history"]);
    // Each result whole in the request right after its call (older rounds
    // are compacted to line 1 by the loop).
    let latest = |i: usize| tool_messages(&bodies[i]).pop().unwrap_or_default();
    for (i, run, kind, n, mean) in [
        (2, XLAB_STRATEGY, "weekend_window", 1, XLAB_FADE_MEAN_BPS),
        (3, "matrix_move", "move_trigger", 2, XLAB_MOVE_MEAN_BPS),
    ] {
        let result = &latest(i);
        let head = regex::Regex::new(&format!(
            r"^backtest (\d{{8}}T\d{{6}}Z-{run}) {run} {kind} 1h research n={n} mean_net_bps={} ",
            regex::escape(&format!("{mean:+.2}"))
        ))
        .unwrap();
        let id = head
            .captures(result)
            .unwrap_or_else(|| panic!("line 1 of {run}: {result}"))[1]
            .to_string();
        let dir = ws.home.join("state/engine-matrix/backtests").join(&id);
        assert!(
            result.ends_with(&format!(
                "\nrows: backtest {{\"run_id\": \"{id}\", \"view\": \"periods\"}} (or \"instruments\", \
                 \"trades\", \"notes\") — the run's files are outside your workspace"
            )) && dir.is_dir(),
            "{result}"
        );
        assert!(!result.contains(&dir.display().to_string()), "{result}");
        assert!(
            result.contains("\nresearch n=") && result.contains("\ncapped n="),
            "{result}"
        );
        assert!(
            result.len() < 8_192
                && !result.contains("bytes in observation")
                && !result.contains("[truncated"),
            "{} chars: {result}",
            result.len()
        );
    }
    let result = &latest(1);
    assert!(
        result.starts_with(
            "mkt_history hyperliquid:xyz:TSLA 1h bars=67 2026-09-25T20:00:00Z … \
             2026-09-28T14:00:00Z last_close=360.2 ret_bps=-331.2 | ok "
        ),
        "{result}"
    );
    assert!(
        result.contains("bars (48 of 67, evenly sampled") && result.contains("; data holds 67"),
        "{result}"
    );
    assert!(
        result.contains("2026-09-28T14:00:00Z  363.54"),
        "the last bar: {result}"
    );
    assert!(result.contains("funding_points=67"), "{result}");
    assert!(
        result.len() < 8_192
            && !result.contains("bytes in observation")
            && !result.contains("[truncated"),
        "{} chars: {result}",
        result.len()
    );
}

/// The xlab_holdout set on the local engine, scripted, through `run-agent`:
/// the seeded stored run's trades are read by its run id, its holdout shown
/// (a read), then a split hides the holdout (the in-sample run, `holdout
/// hidden`) and a read shows both halves (the split's second read) — each
/// text whole under the 16k agent's 8 192-char cap.
#[test]
fn offline_local_xlab_holdout() {
    let ws = workspace();
    let prep = prepare(MOCK, Set::XlabHoldout, &ws, &[]);
    let (leg, bodies) = offline_leg(
        Set::XlabHoldout,
        &ws,
        &prep,
        vec![
            tool_call_reply(
                "c1",
                "backtest",
                &json!({"run_id": XLAB_STORED_RUN, "view": "trades", "holdout": true}),
            ),
            tool_call_reply(
                "c2",
                "backtest",
                &json!({"spec": xlab_spec(), "from": XLAB_FROM, "to": XLAB_TO,
                        "split": XLAB_SPLIT}),
            ),
            tool_call_reply(
                "c3",
                "backtest",
                &json!({"spec": xlab_spec(), "from": XLAB_FROM, "to": XLAB_TO,
                        "split": XLAB_SPLIT, "holdout": true}),
            ),
            text_reply(&format!(
                "gross_bps={XLAB_WORST_GROSS_BPS:+.2} mean_net_bps={XLAB_IN_SAMPLE_MEAN_BPS:+.2} \
                 holdout mean_net_bps={XLAB_HOLDOUT_MEAN_BPS:+.2}"
            )),
        ],
    );
    assert_leg(MOCK, Set::XlabHoldout, &leg, &ws, &prep);
    assert_eq!(bodies.len(), 4, "{}", leg.context());
    let mut names = advertised(&bodies[0]);
    names.sort();
    assert_eq!(names, ["backtest", "compress_and_store"]);
    let latest = |i: usize| tool_messages(&bodies[i]).pop().unwrap_or_default();
    let rows = &latest(1);
    assert!(
        rows.starts_with(&format!(
            "backtest rows {XLAB_STORED_RUN} view=trades · conf_rows move_trigger 1h \
             arm=research · 2 trade(s)"
        )) && rows.contains(
            "\nsplit time:2026-09-27T00:00:00Z: both halves shown (1 holdout trade(s)) · holdout \
             read #1 for this spec · 1 read(s) of split"
        ) && rows.contains(" gross_bps=-52.88 ")
            && rows.contains("\nall 2 trade(s) by Σ net USD, best first:"),
        "{rows}"
    );
    let hidden = &latest(2);
    for want in [
        " matrix_move move_trigger 1h research n=1 mean_net_bps=+2.07 ",
        "\nholdout hidden: split time:2026-09-27T00:00:00Z — no decision at or after the split",
    ] {
        assert!(hidden.contains(want), "{want}: {hidden}");
    }
    assert!(!hidden.contains("holdout n="), "{hidden}");
    let read = &latest(3);
    for want in [
        &format!(
            " matrix_move move_trigger 1h HOLDOUT of time:2026-09-27T00:00:00Z (out-of-sample) \
             research: n=1 mean_net_bps={XLAB_HOLDOUT_MEAN_BPS:+.2} "
        ),
        "\nin-sample (already seen while tuning — reference only) research: n=1 mean_net_bps=+2.07",
        "\nholdout read #1 for this spec · 2 read(s) of split time:2026-09-27T00:00:00Z",
    ] {
        assert!(read.contains(want), "{want}: {read}");
    }
    for result in [hidden, read, rows] {
        assert!(
            result.len() < 8_192
                && !result.contains("bytes in observation")
                && !result.contains("[truncated"),
            "{} chars: {result}",
            result.len()
        );
    }
}

/// The soe set on the local engine, scripted, through `run-agent`: the
/// week's candidates read (the carried one, its inputs and computed
/// verdict), the fixture proposal written into the week open in `PROPOSE`,
/// the fixture challenge into the replay open in `CHALLENGE` — each record
/// stamped with the step's agent, each text whole under the 16k agent's
/// 8 192-char cap, and the model sees only the composed set.
#[test]
fn offline_local_soe() {
    let ws = workspace();
    let prep = prepare(MOCK, Set::Soe, &ws, &[]);
    let (leg, bodies) = offline_leg(
        Set::Soe,
        &ws,
        &prep,
        vec![
            tool_call_reply(
                "c1",
                "soe_view",
                &json!({"run": SOE_RUN, "view": "candidates"}),
            ),
            tool_call_reply(
                "c2",
                "soe_propose",
                &json!({"run": SOE_RUN, "proposal": soe_draft("proposal.json")}),
            ),
            tool_call_reply("c3", "soe_challenge", &soe_challenge_args(SOE_REPLAY)),
            text_reply(&format!("{SOE_PROPOSAL_ID} {SOE_CHALLENGE_ID}")),
        ],
    );
    assert_leg(MOCK, Set::Soe, &leg, &ws, &prep);
    assert_eq!(bodies.len(), 4, "{}", leg.context());
    let mut names = advertised(&bodies[0]);
    names.sort();
    assert_eq!(
        names,
        [
            "compress_and_store",
            "soe_challenge",
            "soe_propose",
            "soe_view"
        ]
    );
    let latest = |i: usize| tool_messages(&bodies[i]).pop().unwrap_or_default();
    for (i, head) in [
        (
            1,
            "soe_view cycles/2026-W42 candidates OPEN PROPOSE decided_at=2026-10-12T12:00:00Z shown=1 of 1",
        ),
        (
            2,
            "soe_propose 2026-W42.p01 news-automation v1 AUTOMATE run=cycles/2026-W42 | written",
        ),
        (
            3,
            "soe_challenge 2026-W42.c01 target=news-automation kind=HIDDEN_LABOR effect=WIDEN",
        ),
    ] {
        let result = latest(i);
        assert!(result.starts_with(head), "result {i}: {result}");
        assert!(
            result.len() < 8_192
                && !result.contains("bytes in observation")
                && !result.contains("[truncated"),
            "{} chars: {result}",
            result.len()
        );
    }
    assert!(
        latest(1).contains("carried from 2026-W41") && latest(2).contains("verdict HOLD"),
        "{}\n{}",
        latest(1),
        latest(2)
    );
}

/// The sources set on the local engine, scripted, through `run-agent`: the
/// leg's `sources.db` (seeded by `tengu sources import`, no network) read
/// as of before and after the change notice, knowable — the original notice
/// stands, then the change supersedes it (`correction`). Each text arrives
/// whole under the 16k agent's 8 192-char cap: typed lines with full record
/// ids, the buyer's name inside one fence after one system note; the model
/// sees only the composed set.
#[test]
fn offline_local_sources() {
    let ws = workspace();
    let prep = prepare(MOCK, Set::Sources, &ws, &[]);
    let original = stored_record_id(&ws, SOURCES_ORIGINAL).expect("seeded");
    let change = stored_record_id(&ws, SOURCES_CHANGE).expect("seeded");
    let at = |day: &str| json!({"at": day, "mode": "knowable", "source": "ted_search"});
    let (leg, bodies) = offline_leg(
        Set::Sources,
        &ws,
        &prep,
        vec![
            tool_call_reply("c1", "source_evidence", &at(SOURCES_BEFORE)),
            tool_call_reply("c2", "source_evidence", &at(SOURCES_AFTER)),
            text_reply(&format!("{SOURCES_ORIGINAL} {change}")),
        ],
    );
    assert_leg(MOCK, Set::Sources, &leg, &ws, &prep);
    assert_eq!(bodies.len(), 3, "{}", leg.context());
    let mut names = advertised(&bodies[0]);
    names.sort();
    assert_eq!(names, ["compress_and_store", "source_evidence"]);
    let latest = |i: usize| tool_messages(&bodies[i]).pop().unwrap_or_default();
    let (before, after) = (latest(1), latest(2));
    assert!(
        before.starts_with(&format!(
            "source_asof {SOURCES_BEFORE}T00:00:00Z knowable: 1 facts · 0 pending · 0 expired"
        )),
        "{before}"
    );
    assert!(
        before.contains(&format!("\nfact {original} event=ted:procedure:")),
        "{before}"
    );
    assert!(
        !before.contains(&change),
        "the change is not yet public: {before}"
    );
    assert!(
        after.contains(&format!("\nsuperseded {original} by {change} (correction)")),
        "{after}"
    );
    assert!(
        after.contains(&format!("\nfact {change} event=")),
        "{after}"
    );
    for result in [&before, &after] {
        let note = "[System note: each source-text block below quotes external data";
        assert_eq!(result.matches(note).count(), 1, "{result}");
        assert!(
            result.contains("field=\"buyer_name\">") && result.contains("</source-text>"),
            "{result}"
        );
        assert!(
            result.len() < 8_192
                && !result.contains("bytes in observation")
                && !result.contains("[truncated"),
            "{} chars: {result}",
            result.len()
        );
    }
}

/// The xlab_rank set on the local engine, scripted, through `run-agent` on a
/// copy of the hardened fixture as sandbox `rank-test` + `[strategy_ranking]`
/// (the sealed test contract): `strategy_ranking` runs the date over the
/// seeded warehouse (configured scope + the run-agent grant, no network) and
/// publishes it; `latest` reads it back. Each text reaches the model whole
/// under the 16k agent's 8 192-char cap — line 1 with the full contract id,
/// rows with whole run locators, the files relative to the state dir, never
/// its path — and the model sees only the composed set.
#[test]
fn offline_local_xlab_rank() {
    let ws = workspace();
    let prep = prepare(MOCK, Set::XlabRank, &ws, &[]);
    let (leg, bodies) = offline_leg(
        Set::XlabRank,
        &ws,
        &prep,
        vec![
            tool_call_reply(
                "c1",
                "strategy_ranking",
                &json!({"action": "run", "date": RANK_DATE}),
            ),
            tool_call_reply("c2", "strategy_ranking", &json!({"action": "latest"})),
            text_reply(&format!(
                "rank_fade {RANK_FADE_MEAN_BPS:+.2} rank_follow {RANK_FOLLOW_MEAN_BPS:+.2}"
            )),
        ],
    );
    assert_leg(MOCK, Set::XlabRank, &leg, &ws, &prep);
    assert_eq!(bodies.len(), 3, "{}", leg.context());
    let mut names = advertised(&bodies[0]);
    names.sort();
    assert_eq!(names, ["compress_and_store", "strategy_ranking"]);
    let latest = |i: usize| tool_messages(&bodies[i]).pop().unwrap_or_default();
    let state = ws.home.join("state").join(XM_STATE);
    for (i, head) in [
        (
            1,
            format!(
                "strategy_ranking run {RANK_CONTRACT} {RANK_DATE} COMPLETE · ran now · latest \
                 replaced · 2 ranked, 0 ineligible, 0 failed, 0 dropped\n"
            ),
        ),
        (
            2,
            format!(
                "strategy_ranking latest {RANK_CONTRACT} {RANK_DATE} COMPLETE · the newest \
                 COMPLETE ranking — nothing ran · 2 ranked"
            ),
        ),
    ] {
        let result = &latest(i);
        assert!(result.starts_with(&head), "{result}");
        let rows = regex::Regex::new(&format!(
            r"\n  1\. rank_fade  ci95_lo \S+  mean {}  n 2  run:{XM_STATE}/\d{{8}}T\d{{6}}Z-rank_fade\n  2\. rank_follow  ci95_lo \S+  mean \+{:.2}  n 2  run:{XM_STATE}/\d{{8}}T\d{{6}}Z-rank_follow\n",
            regex::escape(&format!("{RANK_FADE_MEAN_BPS:+.2}")),
            RANK_FOLLOW_MEAN_BPS
        ))
        .unwrap();
        assert!(rows.is_match(result), "{result}");
        assert!(
            result.ends_with(&format!(
                "in state dir `{XM_STATE}` — outside your workspace"
            )),
            "{result}"
        );
        assert!(!result.contains(&state.display().to_string()), "{result}");
        assert!(
            result.len() < 8_192
                && !result.contains("bytes in observation")
                && !result.contains("[truncated"),
            "{} chars: {result}",
            result.len()
        );
    }
}

/// Each fixture family's files share every section but `[agents.*]`; each
/// routable agent holds its family's sets (the open agents also list the
/// shell skill), each private `xm_*` agent the xm set; every fixture loads
/// in tengu: a tool runs in-process (`tengu tool call`) — no model involved.
#[test]
fn fixtures_load_and_agree() {
    for hardened in [true, false] {
        let sets: Vec<Set> = Set::ALL
            .iter()
            .copied()
            .filter(|s| s.hardened() == hardened && *s != Set::Xm)
            .collect();
        let mut shared: Option<toml::Table> = None;
        let targets: &[Target] = if hardened {
            &[GEMINI, CLAUDE, GEMMA]
        } else {
            &[GEMINI, CLAUDE, GEMMA, CODEX]
        };
        for &target in targets {
            let file = if hardened {
                target.fixture
            } else {
                target.open_fixture
            };
            let text = std::fs::read_to_string(fixture(file)).unwrap();
            let mut table: toml::Table = toml::from_str(&text).unwrap();
            let agents = table.remove("agents").expect("[agents.*]");
            for (name, agent) in agents.as_table().unwrap() {
                let list = |key: &str| -> Vec<&str> {
                    agent
                        .get(key)
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
                        .unwrap_or_default()
                };
                // Routable agents: the run-agent sets; `xm_*`: the exec set,
                // without a description (the exec-tool load rule).
                let (want, routable): (Vec<&str>, bool) = if name.starts_with("xm_") {
                    (Set::Xm.tools().to_vec(), false)
                } else {
                    // Each tool once, in set order (two sets may share one).
                    let mut tools: Vec<&str> = Vec::new();
                    for t in sets.iter().flat_map(|s| s.tools()) {
                        if !tools.contains(t) {
                            tools.push(t);
                        }
                    }
                    (tools, true)
                };
                assert_eq!(list("tools"), want, "{file}: agents.{name}.tools");
                let skills: Vec<&str> = sets.iter().flat_map(|s| s.skills()).copied().collect();
                if routable {
                    assert_eq!(list("skill_packages"), skills, "{file}: agents.{name}");
                }
                assert_eq!(
                    agent.get("description").is_some(),
                    routable,
                    "{file}: agents.{name}.description"
                );
            }
            match &shared {
                None => shared = Some(table),
                Some(first) => assert_eq!(
                    first, &table,
                    "{file}: sections other than [agents.*] differ from its family's openrouter.toml"
                ),
            }
        }
    }
    for target in [GEMINI, HAIKU, CLAUDE, GEMMA, CODEX] {
        let checks = [
            (
                target.fixture,
                target.xm_agent,
                "risk_status",
                "{}",
                format!("equity={EQUITY}"),
            ),
            (
                target.open_fixture,
                target.agent,
                "hex_to_uint256",
                r#"{"hex": "0xff"}"#,
                "255".to_string(),
            ),
        ];
        // Codex: the open family only (no hardened fixture).
        let checks = checks
            .into_iter()
            .filter(|(_, _, tool, ..)| target.kind != Kind::Codex || *tool != "risk_status");
        for (file, agent, tool, args, want) in checks {
            let ws = workspace();
            let out = Command::new(env!("CARGO_BIN_EXE_tengu"))
                .args([
                    "tool", "call", "--agent", agent, "--tool", tool, "--args", args,
                ])
                .arg("-c")
                .arg(fixture(file))
                .current_dir(&ws.path)
                .env("TENGU_HOME", &ws.home)
                .env("TENGU_MATRIX_WORKSPACE", &ws.path)
                .env("TENGU_MATRIX_FIXTURES", FIXTURES)
                .env("TENGU_MATRIX_MCP_VALUE", &ws.mcp_token)
                .env_remove("TENGU_CONFIG")
                .env_remove("TENGU_EGRESS")
                .env_remove("TENGU_SECRETS_LOADED")
                .output()
                .expect("spawn tengu tool call");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let result: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
            assert!(
                result["is_error"] == false
                    && result["text"].as_str().is_some_and(|t| t.contains(&want)),
                "{file} as {agent}: {stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

/// `x-engine-parity-audit`: every catalog tool (`tengu tool list`, this
/// build) sits in a tool set, so it has a live leg on every engine — a
/// new catalog row fails here until a set holds it.
#[test]
fn every_catalog_tool_has_a_live_leg() {
    let out = Command::new(env!("CARGO_BIN_EXE_tengu"))
        .args(["tool", "list"])
        .env_remove("TENGU_CONFIG")
        .output()
        .expect("run tengu tool list");
    let catalog: Vec<String> = serde_json::from_slice(&out.stdout).expect("a JSON array");
    let covered: Vec<&str> = Set::ALL.iter().flat_map(|s| s.expected()).collect();
    let missing: Vec<&String> = catalog
        .iter()
        .filter(|t| !covered.contains(&t.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "catalog tools in no engine-matrix tool set — add them to a Set in tests/engine_matrix.rs: {missing:?}"
    );
}
