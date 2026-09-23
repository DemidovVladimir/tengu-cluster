// src/adapters/plugins/crypto/mod.rs
//! Crypto plugin — EVM transaction signing, message signing, and ABI helpers.
//!
//! Provides:
//! - `sign_and_send_transaction` — gated by `ctx.scope.check_wallet()`
//! - `sign_message`              — gated by `ctx.scope.check_wallet()`
//! - `get_wallet_address`        — gated by `ctx.scope.check_wallet()`
//! - `abi_encode`                — pure compute (no scope gate)
//! - `hex_to_uint256`            — pure compute (no scope gate)
//!
//! Wallet access is controlled by `ToolScope::wallets`. During the Phase A
//! migration window the `permissive_scope` helper grants a single canonical
//! wallet label (`DEFAULT_WALLET_LABEL` in `helpers.rs`); per-agent wallet
//! allow-lists arrive with A9 / Phase B.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::domain::message::ToolDef;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};

pub(crate) mod abi_encode;
pub(crate) mod helpers;
pub(crate) mod hex_to_uint256;
pub(crate) mod sign_message;
pub(crate) mod sign_tx;
pub(crate) mod wallet_address;

pub(crate) use abi_encode::AbiEncodeTool;
pub(crate) use hex_to_uint256::HexToUint256Tool;
pub(crate) use sign_message::SignMessageTool;
pub(crate) use sign_tx::SignAndSendTransactionTool;
pub(crate) use wallet_address::GetWalletAddressTool;

/// Tool definitions advertised by the crypto plugin.
///
/// Used by `channel_runtime::compute_base_tools` and `compute_bridge_tools` to
/// advertise crypto tools to the engine before the plugin is instantiated.
pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![
        SignAndSendTransactionTool::new(None).definition().clone(),
        SignMessageTool::new().definition().clone(),
        GetWalletAddressTool::new().definition().clone(),
        AbiEncodeTool::new().definition().clone(),
        HexToUint256Tool::new().definition().clone(),
    ]
}

/// Plugin grouping the five crypto primitive tools.
///
/// The optional `cancel` flag is plumbed through to the transaction waiter so
/// that `sign_and_send_transaction` can bail out when the user triggers `/stop`
/// mid-receipt-poll (preserves pre-migration behaviour).
pub(crate) struct CryptoPlugin {
    pub(crate) cancel: Option<Arc<AtomicBool>>,
}

impl CryptoPlugin {
    pub(crate) fn new(cancel: Option<Arc<AtomicBool>>) -> Self {
        Self { cancel }
    }
}

#[async_trait]
impl ToolPlugin for CryptoPlugin {
    fn name(&self) -> &'static str {
        "crypto"
    }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![
            Arc::new(SignAndSendTransactionTool::new(self.cancel.clone())),
            Arc::new(SignMessageTool::new()),
            Arc::new(GetWalletAddressTool::new()),
            Arc::new(AbiEncodeTool::new()),
            Arc::new(HexToUint256Tool::new()),
        ])
    }
}
