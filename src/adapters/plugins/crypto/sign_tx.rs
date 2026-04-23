// src/adapters/plugins/crypto/sign_tx.rs
//! `sign_and_send_transaction` tool — submit an EVM transaction via Privy.
//!
//! Migrated from `crypto_tool_executor.rs` during Phase A / task A3.
//! Uses `ctx.http` directly — no `block_in_place` bridge.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::adapters::plugins::crypto::helpers::{
    privy_send_transaction, resolve_default_chain_id, wait_for_receipt, DEFAULT_WALLET_LABEL,
};
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::tool_utils::require_str;
use crate::adapters::types::ToolDef;

pub(crate) struct SignAndSendTransactionTool {
    def: ToolDef,
    cancel: Option<Arc<AtomicBool>>,
}

impl SignAndSendTransactionTool {
    pub(crate) fn new(cancel: Option<Arc<AtomicBool>>) -> Self {
        Self {
            def: ToolDef::new(
                "sign_and_send_transaction",
                "Sign and send an EVM transaction via Privy wallet.",
                json!({
                    "type": "object",
                    "properties": {
                        "to": {
                            "type": "string",
                            "description": "Destination address (0x-prefixed)"
                        },
                        "data": {
                            "type": "string",
                            "description": "Transaction calldata (0x-prefixed hex)"
                        },
                        "value": {
                            "type": "string",
                            "description": "Value in wei (decimal string, default: \"0\")"
                        },
                        "chain_id": {
                            "type": "integer",
                            "description": "EVM chain ID. Pass the chain ID defined by the calling skill or workflow; do not substitute a different value."
                        },
                        "wait_for_receipt": {
                            "type": "boolean",
                            "description": "Wait for confirmation (default: true)"
                        }
                    },
                    "required": ["to"]
                }),
            ),
            cancel,
        }
    }
}

#[async_trait]
impl Tool for SignAndSendTransactionTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_wallet(DEFAULT_WALLET_LABEL)?;

        let to = require_str(args, "sign_and_send_transaction", "to")?;
        let data = args.get("data").and_then(|v| v.as_str());
        let value = args.get("value").and_then(|v| v.as_str());
        let expected_chain_id = resolve_default_chain_id();
        let chain_id = match args.get("chain_id").and_then(|v| v.as_u64()) {
            Some(given) if given != expected_chain_id => {
                return Err(anyhow!(
                    "chain_id mismatch: tool received {given} but this platform is configured for chain_id {expected_chain_id} (from CHAIN_ID env). \
                    Retry with chain_id: {expected_chain_id}. \
                    Do NOT pass 11155111 (Sepolia) or any other hard-coded chain — always use the platform chain_id."
                ));
            }
            Some(given) => given,
            None => expected_chain_id,
        };
        let wait = args
            .get("wait_for_receipt")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let tx_hash = privy_send_transaction(ctx.http, to, data, value, chain_id).await?;

        let text = if wait {
            let receipt = wait_for_receipt(ctx.http, &tx_hash, self.cancel.as_ref()).await?;
            let status_hex = receipt["status"].as_str().unwrap_or("0x0");
            let confirmed = status_hex == "0x1";
            let status_str = if confirmed { "confirmed" } else { "reverted" };
            format!(
                "tx_hash: {} | status: {} | chain: {} | to: {}",
                tx_hash, status_str, chain_id, to
            )
        } else {
            format!(
                "tx_hash: {} | status: submitted | chain: {} | to: {}",
                tx_hash, chain_id, to
            )
        };

        Ok(ToolOutput::from(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::plugins::workspace::test_support::TestHarness;
    use crate::adapters::ports::ToolScope;
    use tempfile::TempDir;

    #[tokio::test]
    async fn sign_tx_scope_denies_when_wallets_empty() {
        let tmp = TempDir::new().unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp.path().to_path_buf()],
            wallets: Vec::new(),
            ..Default::default()
        };
        let harness = TestHarness::with_scope(tmp.path(), scope);
        let tool = SignAndSendTransactionTool::new(None);
        let result = tool
            .execute(
                &json!({"to": "0x0000000000000000000000000000000000000000"}),
                &harness.ctx(),
            )
            .await;
        assert!(result.is_err(), "expected scope denial, got: {:?}", result);
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("wallet") || msg.contains("wallets"),
            "expected wallet denial message, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn sign_tx_missing_to_errors() {
        let tmp = TempDir::new().unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp.path().to_path_buf()],
            wallets: vec![DEFAULT_WALLET_LABEL.to_string()],
            ..Default::default()
        };
        let harness = TestHarness::with_scope(tmp.path(), scope);
        let tool = SignAndSendTransactionTool::new(None);
        let result = tool.execute(&json!({}), &harness.ctx()).await;
        assert!(result.is_err(), "expected missing 'to' error");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("'to' is required"),
            "expected missing 'to' message, got: {}",
            msg
        );
    }
}
