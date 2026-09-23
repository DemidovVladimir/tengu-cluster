//! Outbound (driven) adapters — implementations of `crate::ports` and the
//! other side-effecting clients the application uses.

pub(crate) mod bridge_env;
pub(crate) mod egress;
pub(crate) mod engines;
pub(crate) mod mcp_client;
pub(crate) mod memory;
pub(crate) mod noop;
pub(crate) mod prune;
pub(crate) mod scaffold;
pub(crate) mod secrets;
pub(crate) mod shell;
pub(crate) mod subprocess_runner;
pub(crate) mod tools;
