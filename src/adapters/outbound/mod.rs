//! Outbound (driven) adapters — implementations of `crate::ports` and the
//! other side-effecting clients the application uses.

pub(crate) mod backfill;
pub(crate) mod bridge_env;
pub(crate) mod clock;
pub(crate) mod decision_cache;
pub(crate) mod decisions;
pub(crate) mod egress;
pub(crate) mod engines;
pub(crate) mod evidence;
pub(crate) mod history_sqlite;
pub(crate) mod http_class;
pub(crate) mod hyperliquid;
pub(crate) mod lineage;
pub(crate) mod market_data;
pub(crate) mod mcp_client;
pub(crate) mod memory;
pub(crate) mod noop;
pub(crate) mod observations;
pub(crate) mod paper_store;
pub(crate) mod prune;
pub(crate) mod rate_limit;
pub(crate) mod runtime_store;
pub(crate) mod scaffold;
pub(crate) mod secrets;
pub(crate) mod shell;
pub(crate) mod soe;
pub(crate) mod solana;
pub(crate) mod sources;
pub(crate) mod subprocess_runner;
pub(crate) mod tools;
pub(crate) mod trace_store;
