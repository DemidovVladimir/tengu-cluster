//! Solana signer port — who signs the write tools' transactions.
//!
//! | Impl | Signs |
//! |---|---|
//! | `adapters::outbound::solana::privy::PrivySigner` | the wallet: the `[solana] privy_wallet_id` Privy wallet, through the seal proxy's `privy` route — no key on this machine |
//! | `adapters::outbound::solana::signer::LocalKeypair` | a one-shot key made in memory for a new DLMM position account, never stored |
//!
//! A signer never reveals a secret: `Debug` / errors carry the public key only.

use async_trait::async_trait;

use crate::domain::solana::{Pubkey, Signature};

#[async_trait]
pub(crate) trait SolanaSigner: Send + Sync {
    fn pubkey(&self) -> Pubkey;
    /// ed25519 signature over the serialized message bytes. `Err` = could
    /// not sign; the transaction is not sent.
    async fn sign(&self, message: &[u8]) -> anyhow::Result<Signature>;
}
