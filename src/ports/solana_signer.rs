//! Solana signer port — who signs the write tools' transactions. The only
//! impl today is a local ed25519 key (`adapters::outbound::solana::signer`,
//! loaded from `[solana] signer_key_file`, or a one-shot keypair for a new
//! DLMM position). A signer never reveals its secret: `Debug` / errors carry
//! the public key only.

use crate::domain::solana::{Pubkey, Signature};

pub(crate) trait SolanaSigner: Send + Sync {
    fn pubkey(&self) -> Pubkey;
    /// ed25519 signature over the serialized message bytes.
    fn sign(&self, message: &[u8]) -> Signature;
}
