//! Built-in workspace primitive definitions (read_file, list_directory, write_file, run_command).
//!
//! Each function returns a `Vec<ToolDef>` describing the primitives an agent can
//! call. The definitions include JSON Schema parameters, risk levels, and
//! approval requirements used by `ToolPolicyCatalog` and `ToolUseService`.
//!
//! Subsystem tools (e.g., `remember` from memory) are owned by their respective
//! adapter modules, not this catalog.

use crate::domain::capability::{CapabilityId, EffectClass, RegisteredTool};
use serde_json::json;

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
pub(crate) fn build_workspace_tools() -> Vec<RegisteredTool> {
    vec![
        RegisteredTool::new(
            "read_file",
            "Read the contents of a file in the workspace. Supports text files and PDF documents — PDF text is extracted automatically.",
            path_only_schema("File path relative to the workspace root"),
            CapabilityId::new("workspace.read").expect("static capability is valid"),
            EffectClass::Read,
        )
        .with_activity_description("Reading file"),
        RegisteredTool::new(
            "list_directory",
            "List files and directories at a path in the workspace.",
            path_only_schema("Directory path relative to the workspace root. Use '.' for the root."),
            CapabilityId::new("workspace.list").expect("static capability is valid"),
            EffectClass::Read,
        )
        .with_activity_description("Listing directory"),
        RegisteredTool::new(
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
            CapabilityId::new("workspace.write").expect("static capability is valid"),
            EffectClass::Write,
        )
        .with_activity_description("Writing file"),
        RegisteredTool::new(
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
            CapabilityId::new("workspace.shell").expect("static capability is valid"),
            EffectClass::ShellExec,
        )
        .with_activity_description("Running command"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_workspace_tools_returns_four() {
        let tools = build_workspace_tools();
        assert_eq!(tools.len(), 4);
        assert_eq!(tools[0].def.name, "read_file");
        assert_eq!(tools[1].def.name, "list_directory");
        assert_eq!(tools[2].def.name, "write_file");
        assert_eq!(tools[3].def.name, "run_command");
        assert!(tools[3].def.policy.as_ref().unwrap().requires_approval);
    }
}
