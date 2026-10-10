//! Names of the opt-in workspace tools — the values `[agents.<name>]`
//! `workspace_tools` (or `tools`) may use to switch one on. Config validation
//! checks against `WORKSPACE_TOOLS`; the tool catalog
//! (`adapters/outbound/tools/mod.rs`) gates registration on the same names,
//! and its tests fail if the two drift.

pub(crate) const AGENTIC_MEMORY: &str = "agentic_memory";
pub(crate) const SHARED_CACHE: &str = "shared_cache";
pub(crate) const PERSISTENT_STORE: &str = "persistent_store";
pub(crate) const SKILL_DISTILL: &str = "skill_distill";
pub(crate) const APPLY_IMPROVER_PROPOSAL: &str = "apply_improver_proposal";
pub(crate) const MANAGE_SKILL: &str = "manage_skill";

// Solana LP family (`adapters/outbound/tools/solana/`) — typed, cached
// observations; one `solana` plugin serves all of them.
pub(crate) const SOL_PRICE: &str = "sol_price";
pub(crate) const DLMM_POOLS: &str = "dlmm_pools";
pub(crate) const DLMM_POOL: &str = "dlmm_pool";
pub(crate) const DLMM_POSITIONS: &str = "dlmm_positions";
pub(crate) const JUP_PERPS: &str = "jup_perps";
pub(crate) const SOLANA_WALLET: &str = "solana_wallet";
pub(crate) const SOLANA_TX: &str = "solana_tx";
pub(crate) const LP_SNAPSHOT: &str = "lp_snapshot";
pub(crate) const LP_SWAP_PLAN: &str = "lp_swap_plan";
pub(crate) const HEDGE_DECIDE: &str = "hedge_decide";
pub(crate) const LP_DECIDE: &str = "lp_decide";

// Hyperliquid family (`adapters/outbound/tools/hyperliquid/`) — typed,
// cached market reads; one `hyperliquid` plugin serves all of them.
pub(crate) const HL_CTX: &str = "hl_ctx";
pub(crate) const HL_BOOK: &str = "hl_book";

// xmarket risk / paper family (`adapters/outbound/tools/xm/`) — typed rows
// over the paper ledger; one `xm` plugin serves all of them.
pub(crate) const RISK_STATUS: &str = "risk_status";
pub(crate) const PAPER_ORDER: &str = "paper_order";
pub(crate) const PAPER_CLOSE: &str = "paper_close";
pub(crate) const PAPER_POSITIONS: &str = "paper_positions";
pub(crate) const XM_EXITS: &str = "xm_exits";
pub(crate) const XM_WEEKEND_FADE: &str = "xm_weekend_fade";

// xlab research family (`adapters/outbound/tools/xlab/`) — read-only typed
// rows over the sandbox's market-data warehouse (`<state dir>/market.db`);
// one `xlab` plugin serves all of them. Not in `XM_TOOLS`: their rows are
// never cached in a workspace store (ttl 0) and their state is the state
// dir's (`market.db`, `backtests/`), so their holders need not share the
// xmarket workspace.
pub(crate) const MARKET_HISTORY: &str = "market_history";
pub(crate) const BACKTEST: &str = "backtest";
/// Strategy rankings of a `[strategy_ranking]` sandbox (`tools/xlab/rank.rs`):
/// runs the ranking coordinator (`application/ranking/`) or reads a
/// published ranking; its state is the state dir's (`strategy-rankings/`).
pub(crate) const STRATEGY_RANKING: &str = "strategy_ranking";

// Source family (O2, `adapters/outbound/tools/sources/`) — read-only typed
// rows over the sandbox's source store (`<sources state dir>/sources.db`);
// one `sources` plugin. Agents never fetch: the operator does
// (`tengu sources fetch`).
pub(crate) const SOURCE_EVIDENCE: &str = "source_evidence";

// SOE family (O3, `adapters/outbound/tools/soe/`) — the weekly cycle's
// stage tools over the private SOE state root (`<sources state dir>/cycles/`,
// `replays/`): one read, two writes into the open run dir (never a file, a
// contact, a spend or a publish); one `soe` plugin serves all of them.
pub(crate) const SOE_VIEW: &str = "soe_view";
pub(crate) const SOE_PROPOSE: &str = "soe_propose";
pub(crate) const SOE_CHALLENGE: &str = "soe_challenge";

/// A2A client (`adapters/outbound/tools/a2a/`): talk to another agent
/// harness over A2A — a remote of `[a2a.remotes.<name>]` only (`config/a2a.rs`);
/// sends what the model writes out of the sandbox, so opt-in.
pub(crate) const A2A: &str = "a2a";

/// Exec tools: each places orders through the `[risk]` gate inside the tool
/// (`tools/xm/exec_common.rs`: gate + fill + ledger write in one
/// transaction). Only a private agent may hold one — no `description`, not
/// `default`, no webhook endpoint's `agent` (`config/risk.rs`, and again at
/// call time).
pub(crate) const XM_EXEC_TOOLS: &[&str] = &[PAPER_ORDER, PAPER_CLOSE, XM_EXITS, XM_WEEKEND_FADE];

/// Every xmarket tool — the opt-in rows of the `hyperliquid` and `xm`
/// plugins (`catalog_tests` keep the two equal; a new xmarket plugin joins
/// both). An agent holding one writes or reads rows in its workspace's
/// observation store, so it shares the sandbox's one xmarket workspace
/// (`config/xmarket.rs`).
pub(crate) const XM_TOOLS: &[&str] = &[
    HL_CTX,
    HL_BOOK,
    RISK_STATUS,
    PAPER_ORDER,
    PAPER_CLOSE,
    PAPER_POSITIONS,
    XM_EXITS,
    XM_WEEKEND_FADE,
];

// Privy wallet tools (`adapters/outbound/tools/crypto/`) that sign; a
// `[risk]` sandbox turns them off (`config/risk.rs`).
pub(crate) const SIGN_AND_SEND_TRANSACTION: &str = "sign_and_send_transaction";
pub(crate) const SIGN_MESSAGE: &str = "sign_message";

/// Every Privy tool that signs with a wallet (`ToolScope::check_wallet`).
pub(crate) const PRIVY_SIGNING_TOOLS: &[&str] = &[SIGN_AND_SEND_TRANSACTION, SIGN_MESSAGE];

/// Every default tool — on for each agent unless its `tools` lists others —
/// plus the always-on `compress_and_store`. With `WORKSPACE_TOOLS` it is
/// the set an agent's `tools` may name (besides `{server}__{tool}`);
/// `catalog_tests` keep it equal to the catalog's default rows.
pub(crate) const DEFAULT_TOOLS: &[&str] = &[
    "read_file",
    "list_directory",
    "write_file",
    "run_command",
    "memory_ingest",
    "memory_search",
    "http_request",
    SIGN_AND_SEND_TRANSACTION,
    SIGN_MESSAGE,
    "get_wallet_address",
    "abi_encode",
    "hex_to_uint256",
    "skill_resource",
    "view_skill",
    "compress_and_store",
];

/// Every opt-in workspace tool name.
pub(crate) const WORKSPACE_TOOLS: &[&str] = &[
    AGENTIC_MEMORY,
    SHARED_CACHE,
    PERSISTENT_STORE,
    SKILL_DISTILL,
    APPLY_IMPROVER_PROPOSAL,
    MANAGE_SKILL,
    SOL_PRICE,
    DLMM_POOLS,
    DLMM_POOL,
    DLMM_POSITIONS,
    JUP_PERPS,
    SOLANA_WALLET,
    SOLANA_TX,
    LP_SNAPSHOT,
    LP_SWAP_PLAN,
    HEDGE_DECIDE,
    LP_DECIDE,
    SOLANA_CLOSE_TOKEN_ACCOUNTS,
    JUPITER_SWAP,
    DLMM_CLOSE_POSITION,
    DLMM_OPEN_POSITION,
    JUP_PERPS_ORDER,
    HL_CTX,
    HL_BOOK,
    RISK_STATUS,
    PAPER_ORDER,
    PAPER_CLOSE,
    PAPER_POSITIONS,
    XM_EXITS,
    XM_WEEKEND_FADE,
    MARKET_HISTORY,
    BACKTEST,
    STRATEGY_RANKING,
    SOURCE_EVIDENCE,
    SOE_VIEW,
    SOE_PROPOSE,
    SOE_CHALLENGE,
    A2A,
];

// Solana write tools (phase 6b, `adapters/outbound/tools/solana/write_*`):
// `mode = "simulate"` (default) needs no key; `mode = "send"` signs with
// `[solana] signer_key_file`, only for an agent whose scope for the tool
// lists the wallet (`wallets = ["<full pubkey>"]`). Config rules:
// `config::solana`.
pub(crate) const SOLANA_CLOSE_TOKEN_ACCOUNTS: &str = "solana_close_token_accounts";
pub(crate) const JUPITER_SWAP: &str = "jupiter_swap";
pub(crate) const DLMM_OPEN_POSITION: &str = "dlmm_open_position";
pub(crate) const DLMM_CLOSE_POSITION: &str = "dlmm_close_position";
pub(crate) const JUP_PERPS_ORDER: &str = "jup_perps_order";

/// Every tool that can sign and send a Solana transaction.
pub(crate) const SOLANA_WRITE_TOOLS: &[&str] = &[
    SOLANA_CLOSE_TOKEN_ACCOUNTS,
    JUPITER_SWAP,
    DLMM_OPEN_POSITION,
    DLMM_CLOSE_POSITION,
    JUP_PERPS_ORDER,
];

/// The closed world of a Software Opportunity Engine sandbox (`[soe]`,
/// `config/soe.rs`): every tool one of its agents may list — reads, the two
/// stage writes into the open run dir (`soe_propose`, `soe_challenge`), skill
/// docs, and the O2 `source_evidence` read. No write, contact, spend, publish
/// or shell tool.
pub(crate) const SOE_ALLOWED: &[&str] = &[
    SOE_VIEW,
    SOE_PROPOSE,
    SOE_CHALLENGE,
    SOURCE_EVIDENCE,
    "read_file",
    "list_directory",
    "view_skill",
    "skill_resource",
];

/// Tools with an effect outside the workspace store — a `[soe]` sandbox
/// needs a deny-all `[default_scopes.<tool>]` for each (`config/soe.rs`).
pub(crate) const SIDE_EFFECT_TOOLS: &[&str] = &[
    "http_request",
    "write_file",
    "run_command",
    SIGN_AND_SEND_TRANSACTION,
    SIGN_MESSAGE,
];
