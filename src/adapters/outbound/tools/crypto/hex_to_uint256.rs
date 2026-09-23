// src/adapters/outbound/tools/crypto/hex_to_uint256.rs
//! `hex_to_uint256` tool — pure-compute hex → decimal uint256 converter.

use alloy::primitives::U256;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::outbound::tools::args::require_str;
use crate::domain::message::ToolDef;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

pub(crate) struct HexToUint256Tool {
    def: ToolDef,
}

impl HexToUint256Tool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                "hex_to_uint256",
                "Convert hex to decimal uint256.",
                json!({
                    "type": "object",
                    "properties": {
                        "hex": {
                            "type": "string",
                            "description": "0x-prefixed hex string (e.g. '0xe6f7...728c')"
                        }
                    },
                    "required": ["hex"]
                }),
            ),
        }
    }
}

#[async_trait]
impl Tool for HexToUint256Tool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // scope: pure-compute — deterministic string parsing, no side effects.
        let hex = require_str(args, "hex_to_uint256", "hex")?;
        let stripped = hex.strip_prefix("0x").unwrap_or(hex);
        let value = U256::from_str_radix(stripped, 16)
            .map_err(|e| anyhow!("hex_to_uint256: invalid hex — {e}"))?;
        Ok(ToolOutput::from(value.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use tempfile::TempDir;

    #[tokio::test]
    async fn hex_to_uint256_converts_prefixed() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = HexToUint256Tool::new();
        let output = tool
            .execute(&json!({"hex": "0x2a"}), &harness.ctx())
            .await
            .unwrap();
        assert_eq!(output.text, "42");
    }

    #[tokio::test]
    async fn hex_to_uint256_converts_unprefixed() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = HexToUint256Tool::new();
        let output = tool
            .execute(&json!({"hex": "ff"}), &harness.ctx())
            .await
            .unwrap();
        assert_eq!(output.text, "255");
    }

    #[tokio::test]
    async fn hex_to_uint256_rejects_invalid() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let tool = HexToUint256Tool::new();
        let result = tool.execute(&json!({"hex": "0xzz"}), &harness.ctx()).await;
        assert!(result.is_err(), "expected invalid hex error");
    }
}
