//! Crypto tool executor — EVM transaction signing and message signing via Privy.
//!
//! Extracts generic blockchain capabilities from the DeSci-specific adapter into
//! reusable platform primitives that any skill can compose.

use crate::application::ports::ToolExecutionPort;
use crate::domain::tool_result::ToolResultEnvelope;
use alloy::dyn_abi::{DynSolType, DynSolValue};
use alloy::primitives::{Address, I256, U256};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::str::FromStr;
use std::sync::Mutex;
use tengu_core::types::ToolCall;

const PRIVY_API_URL: &str = "https://api.privy.io";
const DEFAULT_CHAIN_ID: u64 = 11155111;
const DEFAULT_SEPOLIA_RPC: &str = "https://ethereum-sepolia-rpc.publicnode.com";

static WALLET_ADDRESS_CACHE: Mutex<Option<String>> = Mutex::new(None);

pub(crate) struct CryptoToolExecutionAdapter {
    client: reqwest::Client,
    fallback_runtime: Option<tokio::runtime::Runtime>,
}

impl CryptoToolExecutionAdapter {
    #[allow(dead_code)]
    pub(crate) fn new() -> Result<Self> {
        Self::with_client(None)
    }

    /// Create with an optional shared `reqwest::Client`. Sharing eliminates
    /// redundant connection pools when multiple agents use crypto tools.
    pub(crate) fn with_client(shared_client: Option<reqwest::Client>) -> Result<Self> {
        let fallback_runtime = if tokio::runtime::Handle::try_current().is_ok() {
            None
        } else {
            Some(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?,
            )
        };
        let client = match shared_client {
            Some(c) => c,
            None => reqwest::Client::builder().build()?,
        };
        Ok(Self {
            client,
            fallback_runtime,
        })
    }

    fn run_async<F, T>(&self, future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(future))
        } else {
            self.fallback_runtime
                .as_ref()
                .expect("no tokio runtime available")
                .block_on(future)
        }
    }

    fn execute_sign_and_send(&self, call: &ToolCall) -> Result<String> {
        let to = call
            .arguments
            .get("to")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("sign_and_send_transaction: missing 'to'"))?;
        let data = call.arguments.get("data").and_then(|v| v.as_str());
        let value = call.arguments.get("value").and_then(|v| v.as_str());
        let chain_id = call
            .arguments
            .get("chain_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_CHAIN_ID);
        let wait = call
            .arguments
            .get("wait_for_receipt")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let client = self.client.clone();
        let to = to.to_string();
        let data = data.map(|s| s.to_string());
        let value = value.map(|s| s.to_string());

        self.run_async(async move {
            let tx_hash =
                privy_send_transaction(&client, &to, data.as_deref(), value.as_deref(), chain_id)
                    .await?;

            if wait {
                let receipt = wait_for_receipt(&client, &tx_hash).await?;
                let status_hex = receipt["status"].as_str().unwrap_or("0x0");
                let confirmed = status_hex == "0x1";
                let summary = if confirmed {
                    format!("Transaction confirmed: {}", tx_hash)
                } else {
                    format!("Transaction reverted: {}", tx_hash)
                };
                let envelope = ToolResultEnvelope::ok("sign_and_send_transaction", &summary)
                    .with_id("tx_hash", &tx_hash)
                    .with_id("chain_id", chain_id.to_string())
                    .with_id("to", &to)
                    .with_raw_response(json!({
                        "tx_hash": tx_hash,
                        "confirmed": confirmed,
                        "status": if confirmed { "success" } else { "reverted" },
                        "receipt": receipt
                    }));
                envelope.to_json_string()
            } else {
                let envelope =
                    ToolResultEnvelope::ok("sign_and_send_transaction", "Transaction submitted")
                        .with_id("tx_hash", &tx_hash)
                        .with_id("chain_id", chain_id.to_string())
                        .with_id("to", &to);
                envelope.to_json_string()
            }
        })
    }

    fn execute_sign_message(&self, call: &ToolCall) -> Result<String> {
        let message = call
            .arguments
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("sign_message: missing 'message'"))?;

        let client = self.client.clone();
        let message = message.to_string();

        self.run_async(async move {
            let signature = privy_personal_sign(&client, &message).await?;
            let address = privy_wallet_address(&client).await?;
            let envelope = ToolResultEnvelope::ok(
                "sign_message",
                format!("Message signed by {}", address),
            )
            .with_id("signature", &signature)
            .with_id("signer", &address);
            envelope.to_json_string()
        })
    }

    fn execute_get_wallet_address(&self) -> Result<String> {
        let client = self.client.clone();
        self.run_async(async move {
            let address = privy_wallet_address(&client).await?;
            let envelope = ToolResultEnvelope::ok(
                "get_wallet_address",
                format!("Wallet address: {}", address),
            )
            .with_id("address", &address);
            envelope.to_json_string()
        })
    }

    fn execute_abi_encode(&self, call: &ToolCall) -> Result<String> {
        let signature = call
            .arguments
            .get("function_signature")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("abi_encode: missing 'function_signature'"))?;
        let args = call
            .arguments
            .get("args")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("abi_encode: missing 'args' array"))?;

        let calldata = abi_encode_function_call(signature, args)?;
        let envelope = ToolResultEnvelope::ok(
            "abi_encode",
            format!("Encoded {}", signature),
        )
        .with_id("calldata", &calldata);
        envelope.to_json_string()
    }
}

impl ToolExecutionPort for CryptoToolExecutionAdapter {
    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        match call.name.as_str() {
            "sign_and_send_transaction" => self.execute_sign_and_send(call),
            "sign_message" => self.execute_sign_message(call),
            "get_wallet_address" => self.execute_get_wallet_address(),
            "abi_encode" => self.execute_abi_encode(call),
            other => bail!("Unknown crypto tool: {}", other),
        }
    }
}

// ---------------------------------------------------------------------------
// Privy API helpers
// ---------------------------------------------------------------------------

async fn privy_wallet_address(client: &reqwest::Client) -> Result<String> {
    if let Some(cached) = WALLET_ADDRESS_CACHE.lock().unwrap().as_ref() {
        return Ok(cached.clone());
    }

    let app_id = std::env::var("PRIVY_APP_ID")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_APP_ID"))?;
    let app_secret = std::env::var("PRIVY_APP_SECRET")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_APP_SECRET"))?;
    let wallet_id = std::env::var("PRIVY_WALLET_ID")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_WALLET_ID"))?;

    let resp = client
        .get(format!("{}/v1/wallets/{}", PRIVY_API_URL, wallet_id))
        .basic_auth(&app_id, Some(&app_secret))
        .header("privy-app-id", &app_id)
        .send()
        .await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;
    if !status.is_success() {
        bail!("Privy wallet lookup failed: {} {:?}", status, body);
    }
    let address = body["address"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Privy response missing address"))?
        .to_string();

    *WALLET_ADDRESS_CACHE.lock().unwrap() = Some(address.clone());
    Ok(address)
}

async fn privy_send_transaction(
    client: &reqwest::Client,
    to: &str,
    data: Option<&str>,
    value: Option<&str>,
    chain_id: u64,
) -> Result<String> {
    let app_id = std::env::var("PRIVY_APP_ID")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_APP_ID"))?;
    let app_secret = std::env::var("PRIVY_APP_SECRET")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_APP_SECRET"))?;
    let wallet_id = std::env::var("PRIVY_WALLET_ID")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_WALLET_ID"))?;

    let hex_value = match value {
        Some(v) => {
            let parsed = alloy::primitives::U256::from_str_radix(v, 10)
                .context("invalid tx value — use decimal wei")?;
            format!("0x{:x}", parsed)
        }
        None => "0x0".to_string(),
    };

    let mut transaction = json!({ "to": to, "value": hex_value });
    if let Some(d) = data {
        transaction["data"] = json!(d);
    }

    let resp = client
        .post(format!("{}/v1/wallets/{}/rpc", PRIVY_API_URL, wallet_id))
        .basic_auth(&app_id, Some(&app_secret))
        .header("privy-app-id", &app_id)
        .json(&json!({
            "method": "eth_sendTransaction",
            "caip2": format!("eip155:{}", chain_id),
            "params": { "transaction": transaction }
        }))
        .send()
        .await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;
    if !status.is_success() {
        bail!("Privy eth_sendTransaction failed: {} {:?}", status, body);
    }
    body["data"]["hash"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Privy response missing data.hash"))
        .map(String::from)
}

async fn privy_personal_sign(client: &reqwest::Client, message: &str) -> Result<String> {
    let app_id = std::env::var("PRIVY_APP_ID")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_APP_ID"))?;
    let app_secret = std::env::var("PRIVY_APP_SECRET")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_APP_SECRET"))?;
    let wallet_id = std::env::var("PRIVY_WALLET_ID")
        .map_err(|_| anyhow::anyhow!("Missing environment variable PRIVY_WALLET_ID"))?;

    let resp = client
        .post(format!("{}/v1/wallets/{}/rpc", PRIVY_API_URL, wallet_id))
        .basic_auth(&app_id, Some(&app_secret))
        .header("privy-app-id", &app_id)
        .json(&json!({
            "method": "personal_sign",
            "params": { "message": message, "encoding": "utf-8" }
        }))
        .send()
        .await?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;
    if !status.is_success() {
        bail!("Privy personal_sign failed: {} {:?}", status, body);
    }
    let raw_sig = body["data"]["signature"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Privy response missing data.signature"))?;

    // Normalize v: Privy may return v=0/1, some protocols expect v=27/28.
    let hex = raw_sig.strip_prefix("0x").unwrap_or(raw_sig);
    let mut sig_bytes = alloy::primitives::hex::decode(hex).context("invalid signature hex")?;
    if sig_bytes.len() == 65 && sig_bytes[64] < 27 {
        sig_bytes[64] += 27;
    }
    Ok(format!("0x{}", alloy::primitives::hex::encode(sig_bytes)))
}

// ---------------------------------------------------------------------------
// ABI encoding
// ---------------------------------------------------------------------------

/// ABI-encode a function call from a Solidity-style signature and JSON args.
///
/// Example:
///   signature: "mintReservation(address,uint256,string,string,bytes)"
///   args: ["0xAbC...", "42", "ipfs://Qm...", "VDNA", "0xdead"]
///   → 0x-prefixed hex calldata (selector + encoded params)
fn abi_encode_function_call(signature: &str, args: &[serde_json::Value]) -> Result<String> {
    let open = signature
        .find('(')
        .ok_or_else(|| anyhow::anyhow!("abi_encode: missing '(' in function_signature"))?;
    let close = signature
        .rfind(')')
        .ok_or_else(|| anyhow::anyhow!("abi_encode: missing ')' in function_signature"))?;
    let param_str = &signature[open + 1..close];
    let param_types: Vec<&str> = if param_str.trim().is_empty() {
        vec![]
    } else {
        param_str.split(',').map(|s| s.trim()).collect()
    };

    if param_types.len() != args.len() {
        bail!(
            "abi_encode: function expects {} args but got {}",
            param_types.len(),
            args.len()
        );
    }

    // Compute 4-byte function selector.
    let canonical = format!("{}({})", &signature[..open], param_types.join(","));
    let hash = alloy::primitives::keccak256(canonical.as_bytes());
    let selector = &hash[..4];

    // Parse each argument into a DynSolValue.
    let mut values = Vec::with_capacity(args.len());
    for (i, (type_str, arg)) in param_types.iter().zip(args.iter()).enumerate() {
        let sol_type: DynSolType = type_str.parse().map_err(|e| {
            anyhow::anyhow!(
                "abi_encode: cannot parse type '{}' (arg {}): {}",
                type_str,
                i,
                e
            )
        })?;
        let value = json_arg_to_sol_value(arg, &sol_type, i)?;
        values.push(value);
    }

    // ABI-encode the parameter tuple.
    let encoded_params = DynSolValue::Tuple(values).abi_encode_params();

    let mut calldata = Vec::with_capacity(4 + encoded_params.len());
    calldata.extend_from_slice(selector);
    calldata.extend_from_slice(&encoded_params);

    Ok(format!("0x{}", alloy::primitives::hex::encode(calldata)))
}

/// Convert a JSON argument to a `DynSolValue` according to the expected Solidity type.
fn json_arg_to_sol_value(
    arg: &serde_json::Value,
    sol_type: &DynSolType,
    index: usize,
) -> Result<DynSolValue> {
    let arg_str = match arg {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    };
    let ctx = || format!("arg {}", index);

    match sol_type {
        DynSolType::Address => {
            let addr = Address::from_str(&arg_str)
                .with_context(|| format!("{}: invalid address '{}'", ctx(), arg_str))?;
            Ok(DynSolValue::Address(addr))
        }
        DynSolType::Uint(bits) => {
            let value = parse_uint256(&arg_str)
                .with_context(|| format!("{}: invalid uint{} '{}'", ctx(), bits, arg_str))?;
            Ok(DynSolValue::Uint(value, *bits))
        }
        DynSolType::Int(bits) => {
            let value = I256::from_str(&arg_str)
                .with_context(|| format!("{}: invalid int{} '{}'", ctx(), bits, arg_str))?;
            Ok(DynSolValue::Int(value, *bits))
        }
        DynSolType::Bool => {
            let value = arg_str
                .parse::<bool>()
                .with_context(|| format!("{}: invalid bool '{}'", ctx(), arg_str))?;
            Ok(DynSolValue::Bool(value))
        }
        DynSolType::String => Ok(DynSolValue::String(arg_str)),
        DynSolType::Bytes => {
            let hex = arg_str.strip_prefix("0x").unwrap_or(&arg_str);
            let bytes = alloy::primitives::hex::decode(hex)
                .with_context(|| format!("{}: invalid bytes hex '{}'", ctx(), arg_str))?;
            Ok(DynSolValue::Bytes(bytes))
        }
        DynSolType::FixedBytes(size) => {
            let hex = arg_str.strip_prefix("0x").unwrap_or(&arg_str);
            let mut bytes = alloy::primitives::hex::decode(hex)
                .with_context(|| format!("{}: invalid bytes{} hex '{}'", ctx(), size, arg_str))?;
            if bytes.len() < *size {
                bytes.resize(*size, 0);
            }
            Ok(DynSolValue::FixedBytes(
                alloy::primitives::FixedBytes::from_slice(&bytes[..*size]),
                *size,
            ))
        }
        DynSolType::Array(inner) => {
            let arr = arg.as_array().ok_or_else(|| {
                anyhow::anyhow!("{}: expected JSON array for {}", ctx(), sol_type)
            })?;
            let mut values = Vec::with_capacity(arr.len());
            for (j, elem) in arr.iter().enumerate() {
                values.push(json_arg_to_sol_value(elem, inner, j)?);
            }
            Ok(DynSolValue::Array(values))
        }
        DynSolType::Tuple(inners) => {
            let arr = arg
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("{}: expected JSON array for tuple", ctx()))?;
            if arr.len() != inners.len() {
                bail!(
                    "{}: tuple expects {} elements, got {}",
                    ctx(),
                    inners.len(),
                    arr.len()
                );
            }
            let mut values = Vec::with_capacity(arr.len());
            for (j, (elem, inner)) in arr.iter().zip(inners.iter()).enumerate() {
                values.push(json_arg_to_sol_value(elem, inner, j)?);
            }
            Ok(DynSolValue::Tuple(values))
        }
        other => bail!("{}: unsupported Solidity type '{}'", ctx(), other),
    }
}

/// Parse a decimal or 0x-prefixed hex string into U256.
fn parse_uint256(s: &str) -> Result<U256> {
    if let Some(hex) = s.strip_prefix("0x") {
        U256::from_str_radix(hex, 16).context("invalid hex uint256")
    } else {
        U256::from_str_radix(s, 10).context("invalid decimal uint256")
    }
}

async fn wait_for_receipt(client: &reqwest::Client, tx_hash: &str) -> Result<serde_json::Value> {
    let rpc_url = std::env::var("EVM_RPC_URL").unwrap_or_else(|_| DEFAULT_SEPOLIA_RPC.to_string());

    for _ in 0..90 {
        let resp = client
            .post(&rpc_url)
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "eth_getTransactionReceipt",
                "params": [tx_hash],
                "id": 1
            }))
            .send()
            .await?;
        let body: serde_json::Value = resp.json().await?;
        if let Some(result) = body.get("result") {
            if !result.is_null() {
                return Ok(result.clone());
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    bail!("Transaction receipt not found after 180s: {}", tx_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_encode_simple_function() {
        // transfer(address,uint256)
        let result = abi_encode_function_call(
            "transfer(address,uint256)",
            &[
                json!("0x0000000000000000000000000000000000000001"),
                json!("100"),
            ],
        )
        .unwrap();
        assert!(result.starts_with("0x"));
        // selector for transfer(address,uint256) = 0xa9059cbb
        assert!(result.starts_with("0xa9059cbb"));
        // 4 bytes selector + 2 * 32 bytes params = 68 bytes = 136 hex chars + "0x"
        assert_eq!(result.len(), 2 + 136);
    }

    #[test]
    fn abi_encode_with_string_and_bytes() {
        let result = abi_encode_function_call(
            "mintReservation(address,uint256,string,string,bytes)",
            &[
                json!("0x0000000000000000000000000000000000000001"),
                json!("42"),
                json!("ipfs://QmTest"),
                json!("VDNA"),
                json!("0xdead"),
            ],
        )
        .unwrap();
        assert!(result.starts_with("0x"));
        // selector is 4 bytes = 8 hex chars
        assert!(result.len() > 10);
    }

    #[test]
    fn abi_encode_no_args() {
        let result = abi_encode_function_call("pause()", &[]).unwrap();
        // Just 4-byte selector
        assert_eq!(result.len(), 2 + 8); // "0x" + 8 hex chars
    }

    #[test]
    fn abi_encode_arg_count_mismatch() {
        let err = abi_encode_function_call("transfer(address,uint256)", &[json!("0x01")]);
        assert!(err.is_err());
        assert!(err
            .unwrap_err()
            .to_string()
            .contains("expects 2 args but got 1"));
    }

    #[test]
    fn abi_encode_hex_uint256() {
        let result = abi_encode_function_call("setValue(uint256)", &[json!("0xff")]).unwrap();
        assert!(result.starts_with("0x"));
    }

    #[test]
    fn abi_encode_bool_arg() {
        let result = abi_encode_function_call("setApproval(bool)", &[json!("true")]).unwrap();
        assert!(result.starts_with("0x"));
    }
}
