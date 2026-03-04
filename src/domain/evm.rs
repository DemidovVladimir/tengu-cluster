//! Domain types for EVM signing and transaction submission.
//!
//! Pure business types with no infrastructure dependencies.
//! Used by the `EvmPort` trait (application layer) and the signer adapter.

use serde::{Deserialize, Serialize};

/// A request to send an EVM transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvmTransactionRequest {
    /// Recipient address (hex, 0x-prefixed).
    pub to: String,
    /// Calldata (hex, 0x-prefixed). Empty for plain ETH transfers.
    #[serde(default)]
    pub data: Option<String>,
    /// Value in wei as a decimal string (e.g. "1000000000000000000" for 1 ETH).
    #[serde(default)]
    pub value: Option<String>,
    /// Chain ID. Defaults to 1 (mainnet) if not specified.
    #[serde(default)]
    pub chain_id: Option<u64>,
}

/// A receipt returned after a transaction is mined.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvmTransactionReceipt {
    /// Transaction hash (hex, 0x-prefixed).
    pub transaction_hash: String,
    /// Block number the transaction was included in.
    pub block_number: u64,
    /// Whether the transaction executed successfully.
    pub success: bool,
    /// Gas consumed by the transaction.
    pub gas_used: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_request_serialization_roundtrip() {
        let tx = EvmTransactionRequest {
            to: "0x742d35Cc6634C0532925a3b844Bc9e7595f2bD18".into(),
            data: Some("0xabcdef".into()),
            value: Some("1000000000000000000".into()),
            chain_id: Some(1),
        };
        let json = serde_json::to_string(&tx).unwrap();
        let recovered: EvmTransactionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(recovered.to, tx.to);
        assert_eq!(recovered.data, tx.data);
        assert_eq!(recovered.value, tx.value);
        assert_eq!(recovered.chain_id, tx.chain_id);
    }

    #[test]
    fn transaction_request_defaults() {
        let json = r#"{"to": "0xdead"}"#;
        let tx: EvmTransactionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(tx.to, "0xdead");
        assert!(tx.data.is_none());
        assert!(tx.value.is_none());
        assert!(tx.chain_id.is_none());
    }

    #[test]
    fn transaction_receipt_serialization_roundtrip() {
        let receipt = EvmTransactionReceipt {
            transaction_hash: "0xabc123".into(),
            block_number: 12345,
            success: true,
            gas_used: 21000,
        };
        let json = serde_json::to_string(&receipt).unwrap();
        let recovered: EvmTransactionReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(recovered.transaction_hash, receipt.transaction_hash);
        assert_eq!(recovered.block_number, receipt.block_number);
        assert_eq!(recovered.success, receipt.success);
        assert_eq!(recovered.gas_used, receipt.gas_used);
    }
}
