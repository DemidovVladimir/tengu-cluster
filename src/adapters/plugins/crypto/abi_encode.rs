// src/adapters/plugins/crypto/abi_encode.rs
//! `abi_encode` tool — pure-compute Solidity ABI encoder.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::plugins::crypto::helpers::abi_encode_function_call;
use crate::adapters::tool_utils::require_str;
use crate::domain::message::ToolDef;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

pub(crate) struct AbiEncodeTool {
    def: ToolDef,
}

impl AbiEncodeTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                "abi_encode",
                "ABI-encode an EVM function call.",
                json!({
                    "type": "object",
                    "properties": {
                        "function_signature": {
                            "type": "string",
                            "description": "Solidity function signature, e.g. 'mintReservation(address,uint256,string,string,bytes)'"
                        },
                        "args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Arguments as strings: address='0x...', uint256=decimal or '0x' hex, bytes='0x...' hex, string=plain text, bool='true'/'false'"
                        }
                    },
                    "required": ["function_signature", "args"]
                }),
            ),
        }
    }
}

#[async_trait]
impl Tool for AbiEncodeTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // scope: pure-compute — no fs/net/shell/wallet access, deterministic hashing + encoding.
        let signature = require_str(args, "abi_encode", "function_signature")?;
        let call_args = args
            .get("args")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("abi_encode: missing 'args' array"))?;

        let calldata = abi_encode_function_call(signature, call_args)?;
        Ok(ToolOutput::from(format!("calldata: {}", calldata)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::plugins::workspace::test_support::TestHarness;
    use tempfile::TempDir;

    #[tokio::test]
    async fn abi_encode_transfer_basic() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = AbiEncodeTool::new();
        let output = tool
            .execute(
                &json!({
                    "function_signature": "transfer(address,uint256)",
                    "args": [
                        "0x0000000000000000000000000000000000000001",
                        "42"
                    ]
                }),
                &harness.ctx(),
            )
            .await
            .unwrap();
        // `transfer(address,uint256)` selector is 0xa9059cbb; calldata prefix must match.
        assert!(
            output.text.starts_with("calldata: 0xa9059cbb"),
            "unexpected selector: {}",
            output.text
        );
        // 4 bytes selector + 32 + 32 = 68 bytes → 136 hex chars + "0x" + "calldata: "
        assert!(
            output
                .text
                .contains("000000000000000000000000000000000000000000000000000000000000002a"),
            "expected uint256 = 42 tail, got: {}",
            output.text
        );
    }

    #[tokio::test]
    async fn abi_encode_arity_mismatch_errors() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = AbiEncodeTool::new();
        let result = tool
            .execute(
                &json!({
                    "function_signature": "transfer(address,uint256)",
                    "args": ["0x0000000000000000000000000000000000000001"]
                }),
                &harness.ctx(),
            )
            .await;
        assert!(
            result.is_err(),
            "expected arity mismatch, got: {:?}",
            result
        );
    }
}
