// src/adapters/plugins/crypto/sign_message.rs
//! `sign_message` tool — EIP-191 personal_sign via Privy.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::outbound::tools::args::require_str;
use crate::adapters::outbound::tools::crypto::helpers::{
    privy_personal_sign, privy_wallet_address, DEFAULT_WALLET_LABEL,
};
use crate::domain::message::ToolDef;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

pub(crate) struct SignMessageTool {
    def: ToolDef,
}

impl SignMessageTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                "sign_message",
                "Sign a message via Privy wallet.",
                json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "The message to sign"
                        }
                    },
                    "required": ["message"]
                }),
            ),
        }
    }
}

#[async_trait]
impl Tool for SignMessageTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_wallet(DEFAULT_WALLET_LABEL)?;

        let message = require_str(args, "sign_message", "message")?;

        let signature = privy_personal_sign(ctx.http, message).await?;
        let address = privy_wallet_address(ctx.http).await?;
        Ok(ToolOutput::from(format!(
            "signature: {} | signer: {}",
            signature, address
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use crate::domain::scope::ToolScope;
    use tempfile::TempDir;

    #[tokio::test]
    async fn sign_message_scope_denies_when_wallets_empty() {
        let tmp = TempDir::new().unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp.path().to_path_buf()],
            wallets: Vec::new(),
            ..Default::default()
        };
        let harness = TestHarness::with_scope(tmp.path(), scope);
        let tool = SignMessageTool::new();
        let result = tool
            .execute(&json!({"message": "hello"}), &harness.ctx())
            .await;
        assert!(result.is_err(), "expected scope denial, got: {:?}", result);
    }
}
