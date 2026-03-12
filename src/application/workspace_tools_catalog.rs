//! Built-in workspace primitive definitions (read_file, list_directory, write_file, run_command).
//!
//! Each function returns a `Vec<ToolDef>` describing the primitives an agent can
//! call. The definitions include JSON Schema parameters, risk levels, and
//! approval requirements used by `ToolPolicyCatalog` and `ToolUseService`.
//!
//! Subsystem tools (e.g., `remember` from memory) are owned by their respective
//! adapter modules, not this catalog.

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
            "Write content to a file in the workspace. Creates parent directories if needed.",
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
            "Execute a shell command in the workspace directory and return its output. Use this to run scripts, install packages, call APIs, compile code, or perform any action the user requests. Always prefer executing commands directly over creating script files.",
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

/// Filter workspace tools by an allowlist.
///
/// If `allowed` is `None`, all tools pass through. If `Some(names)`, only
/// tools whose name appears in the list are kept.
pub(crate) fn filter_tools_by_allowlist(
    tools: Vec<ToolDef>,
    allowed: Option<&[String]>,
) -> Vec<ToolDef> {
    match allowed {
        None => tools,
        Some(names) => tools
            .into_iter()
            .filter(|t| names.iter().any(|n| n == &t.name))
            .collect(),
    }
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
