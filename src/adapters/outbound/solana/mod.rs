//! Solana outbound — JSON-RPC client, cache-through account reads, SPL decoders, JSON HTTP fetch,
//! and the write path: local ed25519 signer, cross-process write store (lease / pending / fence),
//! send pipeline (simulate → sign → record → send → confirm).

pub(crate) mod accounts;
pub(crate) mod http_json;
pub(crate) mod layouts;
pub(crate) mod plan;
pub(crate) mod rpc;
pub(crate) mod send;
pub(crate) mod signer;
#[cfg(test)]
pub(crate) mod test_chain;
pub(crate) mod writes_store;
