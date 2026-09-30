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

/// Exec tools: each places orders through the `[risk]` gate inside the tool
/// (`tools/xm/exec_common.rs`: gate + fill + ledger write in one
/// transaction). Only a private agent may hold one — no `description`, not
/// `default`, no webhook endpoint's `agent` (`config/risk.rs`, and again at
/// call time).
pub(crate) const XM_EXEC_TOOLS: &[&str] = &[PAPER_ORDER, PAPER_CLOSE, XM_EXITS];

// Privy wallet tools (`adapters/outbound/tools/crypto/`) that sign; a
// `[risk]` sandbox turns them off (`config/risk.rs`).
pub(crate) const SIGN_AND_SEND_TRANSACTION: &str = "sign_and_send_transaction";
pub(crate) const SIGN_MESSAGE: &str = "sign_message";

/// Every Privy tool that signs with a wallet (`ToolScope::check_wallet`).
pub(crate) const PRIVY_SIGNING_TOOLS: &[&str] = &[SIGN_AND_SEND_TRANSACTION, SIGN_MESSAGE];

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
