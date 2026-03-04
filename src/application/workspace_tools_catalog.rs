use serde_json::json;
use tengu_core::types::{ToolDef, ToolPolicyMetadata, ToolRiskLevel};

fn tool_def(
    name: &str,
    description: &str,
    parameters: serde_json::Value,
    risk_level: ToolRiskLevel,
    requires_approval: bool,
) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: description.into(),
        parameters,
        policy: Some(ToolPolicyMetadata {
            risk_level,
            requires_approval,
        }),
    }
}

fn path_only_schema(path_description: &str) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": path_description
            }
        },
        "required": ["path"]
    })
}

/// Build the set of workspace tool definitions to pass to engine.run().
pub(crate) fn build_workspace_tools() -> Vec<ToolDef> {
    vec![
        tool_def(
            "read_file",
            "Read the contents of a file in the workspace. Supports text files and PDF documents — PDF text is extracted automatically.",
            path_only_schema("File path relative to the workspace root"),
            ToolRiskLevel::Low,
            false,
        ),
        tool_def(
            "list_directory",
            "List files and directories at a path in the workspace.",
            path_only_schema("Directory path relative to the workspace root. Use '.' for the root."),
            ToolRiskLevel::Low,
            false,
        ),
        tool_def(
            "write_file",
            "Write content to a file in the workspace. Creates parent directories if needed. Requires user approval.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the workspace root"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
            ToolRiskLevel::Medium,
            true,
        ),
        tool_def(
            "run_command",
            "Execute a shell command in the workspace directory and return its output. Use this to run scripts, install packages, call APIs, compile code, or perform any action the user requests. Always prefer executing commands directly over creating script files. Requires user approval.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute (runs via sh -c)"
                    }
                },
                "required": ["command"]
            }),
            ToolRiskLevel::High,
            true,
        ),
    ]
}

/// Build memory tool definitions for the vector memory subsystem.
pub(crate) fn build_memory_tools() -> Vec<ToolDef> {
    vec![tool_def(
        "remember",
        "Store a fact or insight in long-term memory for future retrieval across sessions.",
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The fact, insight, or information to remember"
                }
            },
            "required": ["content"]
        }),
        ToolRiskLevel::Low,
        false,
    )]
}

/// Build EVM tool definitions for the on-chain signing subsystem.
#[cfg(feature = "evm")]
pub(crate) fn build_evm_tools() -> Vec<ToolDef> {
    vec![
        tool_def(
            "evm_get_address",
            "Return the wallet's checksummed Ethereum address derived from the configured private key.",
            json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            ToolRiskLevel::High,
            true,
        ),
        tool_def(
            "evm_sign_message",
            "Sign an arbitrary message with the wallet's private key and return the hex-encoded signature (EIP-191 personal_sign).",
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
            ToolRiskLevel::High,
            true,
        ),
        tool_def(
            "evm_send_transaction",
            "Build, sign, and submit an Ethereum transaction. Returns the transaction receipt once mined. Requires user approval.",
            json!({
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Recipient address (hex, 0x-prefixed)"
                    },
                    "data": {
                        "type": "string",
                        "description": "Calldata (hex, 0x-prefixed). Omit for plain ETH transfers."
                    },
                    "value": {
                        "type": "string",
                        "description": "Value in wei as a decimal string (e.g. '1000000000000000000' for 1 ETH)"
                    },
                    "chain_id": {
                        "type": "integer",
                        "description": "Chain ID (default: 1 for mainnet)"
                    }
                },
                "required": ["to"]
            }),
            ToolRiskLevel::High,
            true,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_workspace_tools_returns_four() {
        let tools = build_workspace_tools();
        assert_eq!(tools.len(), 4);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(tools[1].name, "list_directory");
        assert_eq!(tools[2].name, "write_file");
        assert_eq!(tools[3].name, "run_command");
        assert!(tools[3].policy.as_ref().unwrap().requires_approval);
    }
}
