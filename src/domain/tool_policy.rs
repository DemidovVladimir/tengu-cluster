use std::collections::HashMap;
use tengu_core::types::{ToolDef, ToolPolicyMetadata};

/// Domain catalog of tool policies keyed by tool name.
#[derive(Debug, Clone, Default)]
pub(crate) struct ToolPolicyCatalog {
    policies: HashMap<String, ToolPolicyMetadata>,
}

impl ToolPolicyCatalog {
    /// Build a catalog from the tool definitions exposed to the model.
    pub(crate) fn from_tools(tools: &[ToolDef]) -> Self {
        let mut policies = HashMap::new();
        for tool in tools {
            let policy = tool.policy.unwrap_or_default();
            policies.insert(tool.name.clone(), policy);
        }
        Self { policies }
    }

    /// Whether a tool call should require interactive user approval.
    pub(crate) fn requires_approval(&self, tool_name: &str) -> bool {
        self.policies
            .get(tool_name)
            .map(|policy| policy.requires_approval)
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tengu_core::types::{ToolDef, ToolPolicyMetadata, ToolRiskLevel};

    #[test]
    fn requires_approval_uses_tool_policy_metadata() {
        let tools = vec![
            ToolDef {
                name: "read_file".into(),
                description: "Read".into(),
                parameters: json!({}),
                policy: Some(ToolPolicyMetadata {
                    risk_level: ToolRiskLevel::Low,
                    requires_approval: false,
                }),
            },
            ToolDef {
                name: "write_file".into(),
                description: "Write".into(),
                parameters: json!({}),
                policy: Some(ToolPolicyMetadata {
                    risk_level: ToolRiskLevel::Medium,
                    requires_approval: true,
                }),
            },
        ];

        let catalog = ToolPolicyCatalog::from_tools(&tools);
        assert!(!catalog.requires_approval("read_file"));
        assert!(catalog.requires_approval("write_file"));
        assert!(catalog.requires_approval("unknown_tool"));
    }
}
