//! Solana outbound — JSON-RPC client, cache-through account reads, SPL decoders, JSON HTTP fetch,
//! and the local ed25519 signer of the write tools.

pub(crate) mod accounts;
pub(crate) mod http_json;
pub(crate) mod layouts;
pub(crate) mod plan;
pub(crate) mod rpc;
pub(crate) mod signer;
