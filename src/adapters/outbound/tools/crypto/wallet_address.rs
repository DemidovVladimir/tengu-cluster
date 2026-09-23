// src/adapters/plugins/crypto/wallet_address.rs
//! `get_wallet_address` tool — return the Privy-managed wallet address.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::outbound::tools::crypto::helpers::{
    privy_wallet_address, DEFAULT_WALLET_LABEL,
};
use crate::domain::message::ToolDef;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

pub(crate) struct GetWalletAddressTool {
    def: ToolDef,
}

impl GetWalletAddressTool {
    pub(crate) fn new() -> Self {
        Self {
            def: ToolDef::new(
                "get_wallet_address",
                "Get Privy wallet address.",
                json!({
                    "type": "object",
                    "properties": {}
                }),
            ),
        }
    }
}

#[async_trait]
impl Tool for GetWalletAddressTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, _args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_wallet(DEFAULT_WALLET_LABEL)?;

        let address = privy_wallet_address(ctx.http).await?;
        Ok(ToolOutput::from(format!("address: {}", address)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use crate::domain::scope::ToolScope;
    use tempfile::TempDir;

    #[tokio::test]
    async fn wallet_address_scope_denies_when_wallets_empty() {
        let tmp = TempDir::new().unwrap();
        let scope = ToolScope {
            fs_roots: vec![tmp.path().to_path_buf()],
            wallets: Vec::new(),
            ..Default::default()
        };
        let harness = TestHarness::with_scope(tmp.path(), scope);
        let tool = GetWalletAddressTool::new();
        let result = tool.execute(&json!({}), &harness.ctx()).await;
        assert!(result.is_err(), "expected scope denial, got: {:?}", result);
    }
}
