// src/adapters/plugins/crypto/helpers.rs
//! Shared Privy + ABI helpers used by the crypto plugin tools.
//!
//! Lift-and-shift from `crypto_tool_executor.rs` — behaviour preserved exactly.

use alloy::dyn_abi::{DynSolType, DynSolValue};
use alloy::primitives::{Address, I256, U256};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) const PRIVY_API_URL: &str = "https://api.privy.io";
pub(crate) const DEFAULT_CHAIN_ID: u64 = 11155111;
pub(crate) const DEFAULT_SEPOLIA_RPC: &str = "https://ethereum-sepolia-rpc.publicnode.com";

/// Canonical wallet label used for scope checks during the migration window.
/// Until per-agent wallet allow-lists land in Phase B, all crypto tools share
/// this single label and the `permissive_scope` grants it.
// TODO(Phase B): replace with per-agent wallet allow-lists.
pub(crate) const DEFAULT_WALLET_LABEL: &str = "default";

/// Process-wide cache of the Privy wallet address to avoid hitting the API once
/// per `sign_message` / `get_wallet_address` call.
pub(crate) static WALLET_ADDRESS_CACHE: Mutex<Option<String>> = Mutex::new(None);

pub(crate) async fn privy_wallet_address(client: &reqwest::Client) -> Result<String> {
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

pub(crate) async fn privy_send_transaction(
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

pub(crate) async fn privy_personal_sign(
    client: &reqwest::Client,
    message: &str,
) -> Result<String> {
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

pub(crate) async fn wait_for_receipt(
    client: &reqwest::Client,
    tx_hash: &str,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<serde_json::Value> {
    let rpc_url = std::env::var("EVM_RPC_URL").unwrap_or_else(|_| DEFAULT_SEPOLIA_RPC.to_string());

    for _ in 0..90 {
        if cancel.is_some_and(|f| f.load(Ordering::Relaxed)) {
            bail!("Cancelled by /stop while waiting for receipt: {}", tx_hash);
        }
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

// ---------------------------------------------------------------------------
// ABI encoding
// ---------------------------------------------------------------------------

/// ABI-encode a function call from a Solidity-style signature and JSON args.
///
/// Example:
///   signature: "mintReservation(address,uint256,string,string,bytes)"
///   args: ["0xAbC...", "42", "ipfs://Qm...", "VDNA", "0xdead"]
///   → 0x-prefixed hex calldata (selector + encoded params)
pub(crate) fn abi_encode_function_call(
    signature: &str,
    args: &[serde_json::Value],
) -> Result<String> {
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
pub(crate) fn parse_uint256(s: &str) -> Result<U256> {
    if let Some(hex) = s.strip_prefix("0x") {
        U256::from_str_radix(hex, 16).context("invalid hex uint256")
    } else {
        U256::from_str_radix(s, 10).context("invalid decimal uint256")
    }
}
