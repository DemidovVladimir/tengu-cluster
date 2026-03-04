//! Alloy-based EVM signer adapter.
//!
//! Implements `EvmPort` using the `alloy` crate for private key management,
//! message signing, and transaction submission. Feature-gated behind `--features evm`.

use crate::application::ports::EvmPort;
use crate::domain::evm::{EvmTransactionReceipt, EvmTransactionRequest};
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::{local::PrivateKeySigner, Signer};
use anyhow::{Context, Result};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

/// Adapter wrapping an alloy `PrivateKeySigner` + RPC URL.
pub(crate) struct AlloySigner {
    signer: PrivateKeySigner,
    rpc_url: String,
}

impl AlloySigner {
    /// Create a new signer from a hex-encoded private key and RPC URL.
    ///
    /// Accepts keys with or without the `0x` prefix.
    pub(crate) fn new(private_key_hex: &str, rpc_url: String) -> Result<Self> {
        let key = private_key_hex.strip_prefix("0x").unwrap_or(private_key_hex);
        let signer =
            PrivateKeySigner::from_str(key).context("invalid private key hex")?;
        Ok(Self { signer, rpc_url })
    }
}

impl EvmPort for AlloySigner {
    fn get_address(&self) -> Result<String> {
        Ok(format!("{}", self.signer.address()))
    }

    fn sign_message(
        &self,
        message: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        let msg = message.to_owned();
        Box::pin(async move {
            let sig = self
                .signer
                .sign_message(msg.as_bytes())
                .await
                .context("failed to sign message")?;
            Ok(format!("0x{}", sig))
        })
    }

    fn send_transaction(
        &self,
        tx: &EvmTransactionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<EvmTransactionReceipt>> + Send + '_>> {
        let to_addr = tx.to.clone();
        let data = tx.data.clone();
        let value = tx.value.clone();
        let chain_id = tx.chain_id;
        let rpc_url = self.rpc_url.clone();
        let signer = self.signer.clone();

        Box::pin(async move {
            let url = rpc_url.parse().context("invalid RPC URL")?;
            let provider = ProviderBuilder::new()
                .wallet(signer)
                .connect_http(url);

            let to = Address::from_str(&to_addr).context("invalid 'to' address")?;

            let mut tx_req = alloy::rpc::types::TransactionRequest::default().to(to);

            if let Some(ref data_hex) = data {
                let hex_str = data_hex.strip_prefix("0x").unwrap_or(data_hex);
                let bytes =
                    alloy::primitives::hex::decode(hex_str).context("invalid calldata hex")?;
                tx_req.input =
                    alloy::rpc::types::TransactionInput::new(Bytes::from(bytes));
            }

            if let Some(ref val_str) = value {
                let val = U256::from_str_radix(val_str, 10)
                    .context("invalid value (expected decimal wei string)")?;
                tx_req = tx_req.value(val);
            }

            if let Some(cid) = chain_id {
                tx_req.chain_id = Some(cid);
            }

            let pending = provider
                .send_transaction(tx_req)
                .await
                .context("failed to send transaction")?;

            let receipt = pending
                .get_receipt()
                .await
                .context("failed to get transaction receipt")?;

            Ok(EvmTransactionReceipt {
                transaction_hash: format!("{}", receipt.transaction_hash),
                block_number: receipt.block_number.unwrap_or(0),
                success: receipt.status(),
                gas_used: receipt.gas_used as u64,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known test key (do NOT use in production — hardhat/anvil default key #0).
    const TEST_KEY: &str =
        "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const TEST_KEY_0X: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    #[test]
    fn parse_key_without_prefix() {
        let signer = AlloySigner::new(TEST_KEY, "http://localhost:8545".into());
        assert!(signer.is_ok());
    }

    #[test]
    fn parse_key_with_0x_prefix() {
        let signer = AlloySigner::new(TEST_KEY_0X, "http://localhost:8545".into());
        assert!(signer.is_ok());
    }

    #[test]
    fn invalid_key_rejected() {
        let result = AlloySigner::new("not-a-hex-key", "http://localhost:8545".into());
        assert!(result.is_err());
    }

    #[test]
    fn get_address_returns_hex() {
        let signer = AlloySigner::new(TEST_KEY, "http://localhost:8545".into()).unwrap();
        let addr = signer.get_address().unwrap();
        assert!(addr.starts_with("0x"), "address should be 0x-prefixed");
        assert_eq!(addr.len(), 42, "address should be 42 chars (0x + 40 hex)");
    }

    #[test]
    fn alloy_signer_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AlloySigner>();
    }
}
