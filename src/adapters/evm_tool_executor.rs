//! Adapter bridging EVM tool calls to the EvmPort.
//!
//! Follows the same sync→async bridging pattern as `MemoryToolExecutionAdapter`:
//! owns a dedicated single-threaded tokio runtime and calls `block_on()` for
//! async port methods.

use crate::application::ports::{EvmPort, ToolExecutionPort};
use crate::domain::evm::EvmTransactionRequest;
use anyhow::Result;
use std::sync::Arc;
use tengu_core::types::ToolCall;

/// Adapter implementing `ToolExecutionPort` by routing EVM tool calls through
/// a dedicated tokio runtime (sync → async bridge).
pub(crate) struct EvmToolExecutionAdapter {
    port: Arc<dyn EvmPort>,
    runtime: tokio::runtime::Runtime,
}

impl EvmToolExecutionAdapter {
    pub(crate) fn new(port: Arc<dyn EvmPort>) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self { port, runtime })
    }
}

impl ToolExecutionPort for EvmToolExecutionAdapter {
    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        match call.name.as_str() {
            "evm_get_address" => {
                let addr = self.port.get_address()?;
                Ok(serde_json::json!({ "address": addr }).to_string())
            }
            "evm_sign_message" => {
                let message = call
                    .arguments
                    .get("message")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("evm_sign_message: missing 'message' argument"))?;

                let signature = self.runtime.block_on(self.port.sign_message(message))?;
                Ok(serde_json::json!({ "signature": signature }).to_string())
            }
            "evm_send_transaction" => {
                let to = call
                    .arguments
                    .get("to")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        anyhow::anyhow!("evm_send_transaction: missing 'to' argument")
                    })?;

                let tx = EvmTransactionRequest {
                    to: to.into(),
                    data: call
                        .arguments
                        .get("data")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    value: call
                        .arguments
                        .get("value")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    chain_id: call
                        .arguments
                        .get("chain_id")
                        .and_then(|v| v.as_u64()),
                };

                let receipt = self.runtime.block_on(self.port.send_transaction(&tx))?;
                Ok(serde_json::to_string(&receipt)?)
            }
            other => Err(anyhow::anyhow!("unknown EVM tool: {}", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::evm::{EvmTransactionReceipt, EvmTransactionRequest};
    use std::future::Future;
    use std::pin::Pin;

    /// Mock EvmPort for unit testing.
    struct MockEvmPort;

    impl EvmPort for MockEvmPort {
        fn get_address(&self) -> Result<String> {
            Ok("0x1234567890abcdef1234567890abcdef12345678".into())
        }

        fn sign_message(
            &self,
            message: &str,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
            let msg = message.to_owned();
            Box::pin(async move { Ok(format!("0xsig_{}", msg)) })
        }

        fn send_transaction(
            &self,
            tx: &EvmTransactionRequest,
        ) -> Pin<Box<dyn Future<Output = Result<EvmTransactionReceipt>> + Send + '_>> {
            let to = tx.to.clone();
            Box::pin(async move {
                Ok(EvmTransactionReceipt {
                    transaction_hash: format!("0xtxhash_to_{}", to),
                    block_number: 42,
                    success: true,
                    gas_used: 21000,
                })
            })
        }
    }

    fn make_adapter() -> EvmToolExecutionAdapter {
        EvmToolExecutionAdapter::new(Arc::new(MockEvmPort)).unwrap()
    }

    fn tool_call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "test".into(),
            name: name.into(),
            arguments: args,
        }
    }

    #[test]
    fn get_address_tool() {
        let adapter = make_adapter();
        let call = tool_call("evm_get_address", serde_json::json!({}));
        let result = adapter.execute_tool(&call).unwrap();
        assert!(result.contains("0x1234567890abcdef"));
    }

    #[test]
    fn sign_message_tool() {
        let adapter = make_adapter();
        let call = tool_call("evm_sign_message", serde_json::json!({"message": "hello"}));
        let result = adapter.execute_tool(&call).unwrap();
        assert!(result.contains("0xsig_hello"));
    }

    #[test]
    fn send_transaction_tool() {
        let adapter = make_adapter();
        let call = tool_call(
            "evm_send_transaction",
            serde_json::json!({"to": "0xdead", "value": "1000"}),
        );
        let result = adapter.execute_tool(&call).unwrap();
        assert!(result.contains("0xtxhash_to_0xdead"));
        assert!(result.contains("\"success\":true"));
    }

    #[test]
    fn unknown_tool_returns_error() {
        let adapter = make_adapter();
        let call = tool_call("evm_unknown", serde_json::json!({}));
        let result = adapter.execute_tool(&call);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown EVM tool"));
    }

    #[test]
    fn adapter_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EvmToolExecutionAdapter>();
    }
}
