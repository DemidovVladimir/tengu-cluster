//! Platform primitive tool definitions (http_request, crypto signing).
//!
//! These are generic infrastructure capabilities available to any agent.
//! Skills compose these primitives with domain knowledge to implement
//! specific workflows (e.g., DeSci minting, booking APIs, social posting).

use crate::domain::capability::{CapabilityId, EffectClass, RegisteredTool};
use serde_json::json;

/// Build the set of platform-level primitive tools.
pub(crate) fn build_platform_tools() -> Vec<RegisteredTool> {
    vec![
        RegisteredTool::new(
            "http_request",
            "Make an HTTP request to an external API. Supports JSON and multipart/form-data \
             (file upload). Use this to interact with any REST API documented in active skills.",
            json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Full URL. Supports $ENV_VAR (e.g. $MOLECULE_LABS_URL or https://api.example.com/v1/resource)"
                    },
                    "method": {
                        "type": "string",
                        "description": "HTTP method",
                        "enum": ["GET", "POST", "PUT", "DELETE", "PATCH"]
                    },
                    "headers": {
                        "type": "string",
                        "description": "JSON object of request headers. Use $ENV_VAR for secrets, e.g. {\"Authorization\": \"Bearer $BEACH_API_KEY\"}"
                    },
                    "body": {
                        "type": "string",
                        "description": "Request body — JSON string for application/json, or raw text"
                    },
                    "file_path": {
                        "type": "string",
                        "description": "Workspace-relative path for multipart/form-data file upload"
                    },
                    "file_field_name": {
                        "type": "string",
                        "description": "Form field name for the uploaded file (default: \"file\")"
                    },
                    "auth_bearer_env": {
                        "type": "string",
                        "description": "Env var name for Bearer token auth (e.g. \"BEACH_API_KEY\")"
                    },
                    "auth_basic_user_env": {
                        "type": "string",
                        "description": "Env var name for Basic auth username (e.g. \"PRIVY_APP_ID\")"
                    },
                    "auth_basic_pass_env": {
                        "type": "string",
                        "description": "Env var name for Basic auth password (e.g. \"PRIVY_APP_SECRET\")"
                    }
                },
                "required": ["url", "method"]
            }),
            CapabilityId::new("http.request").expect("static"),
            EffectClass::ExternalApi,
        )
        .with_activity_description("HTTP request"),
        RegisteredTool::new(
            "sign_and_send_transaction",
            "Sign and send an EVM transaction using the configured Privy agentic wallet. \
             Waits for the receipt and returns tx hash + status.",
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
                        "description": "Chain ID (default: 11155111 = Sepolia)"
                    },
                    "wait_for_receipt": {
                        "type": "boolean",
                        "description": "Wait for confirmation (default: true)"
                    }
                },
                "required": ["to"]
            }),
            CapabilityId::new("crypto.sign_tx").expect("static"),
            EffectClass::ChainTx,
        )
        .with_activity_description("Signing transaction")
        .with_required_secrets(&["PRIVY_APP_ID", "PRIVY_APP_SECRET", "PRIVY_WALLET_ID"]),
        RegisteredTool::new(
            "sign_message",
            "Sign a message using the configured Privy agentic wallet. Returns the signature.",
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
            CapabilityId::new("crypto.sign_message").expect("static"),
            EffectClass::ChainTx,
        )
        .with_activity_description("Signing message")
        .with_required_secrets(&["PRIVY_APP_ID", "PRIVY_APP_SECRET", "PRIVY_WALLET_ID"]),
        RegisteredTool::new(
            "get_wallet_address",
            "Get the address of the configured Privy agentic wallet.",
            json!({
                "type": "object",
                "properties": {}
            }),
            CapabilityId::new("crypto.wallet_address").expect("static"),
            EffectClass::Read,
        )
        .with_activity_description("Getting wallet address")
        .with_required_secrets(&["PRIVY_APP_ID", "PRIVY_APP_SECRET", "PRIVY_WALLET_ID"]),
        RegisteredTool::new(
            "abi_encode",
            "ABI-encode an EVM function call. Returns 0x-prefixed hex calldata for use \
             with sign_and_send_transaction. Handles address, uint256, string, bytes, \
             bool, and nested types.",
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
            CapabilityId::new("crypto.abi_encode").expect("static"),
            EffectClass::Read,
        )
        .with_activity_description("ABI-encoding calldata"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_platform_tools_returns_expected() {
        let tools = build_platform_tools();
        assert_eq!(tools.len(), 5);
        let names: Vec<&str> = tools.iter().map(|t| t.def.name.as_str()).collect();
        assert!(names.contains(&"http_request"));
        assert!(names.contains(&"sign_and_send_transaction"));
        assert!(names.contains(&"sign_message"));
        assert!(names.contains(&"get_wallet_address"));
        assert!(names.contains(&"abi_encode"));
    }

    #[test]
    fn http_request_requires_approval() {
        let tools = build_platform_tools();
        let http = tools.iter().find(|t| t.def.name == "http_request").unwrap();
        assert!(http.def.policy.as_ref().unwrap().requires_approval);
    }

    #[test]
    fn get_wallet_address_is_read_only() {
        let tools = build_platform_tools();
        let wallet = tools
            .iter()
            .find(|t| t.def.name == "get_wallet_address")
            .unwrap();
        assert!(!wallet.def.policy.as_ref().unwrap().requires_approval);
    }
}
