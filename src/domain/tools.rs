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
