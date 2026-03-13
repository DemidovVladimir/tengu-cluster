use crate::domain::capability::{CapabilityId, EffectClass, RegisteredTool};
use std::collections::HashMap;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct ToolPolicy {
    pub capability: CapabilityId,
    pub effect_class: EffectClass,
    pub requires_approval: bool,
}

/// Domain catalog of tool policies keyed by tool name.
#[derive(Debug, Clone, Default)]
pub(crate) struct ToolPolicyCatalog {
    policies: HashMap<String, ToolPolicy>,
}

impl ToolPolicyCatalog {
    /// Build a catalog from the tool definitions exposed to the model.
    pub(crate) fn from_tools(tools: &[RegisteredTool]) -> Self {
        let mut policies = HashMap::new();
        for tool in tools {
            let requires_approval = tool
                .def
                .policy
                .as_ref()
                .map(|policy| policy.requires_approval)
                .unwrap_or_else(|| tool.effect_class.requires_approval());
            policies.insert(
                tool.def.name.clone(),
                ToolPolicy {
                    capability: tool.capability.clone(),
                    effect_class: tool.effect_class,
                    requires_approval,
                },
            );
        }
        Self { policies }
    }

    pub(crate) fn is_allowed(&self, tool_name: &str) -> bool {
        self.policies.contains_key(tool_name)
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
    use crate::domain::capability::{CapabilityId, EffectClass, RegisteredTool};

    #[test]
    fn requires_approval_uses_tool_policy_metadata() {
        let tools = vec![
            RegisteredTool::new(
                "read_file",
                "Read",
                serde_json::json!({}),
                CapabilityId::new("workspace.read").unwrap(),
                EffectClass::Read,
            ),
            RegisteredTool::new(
                "write_file",
                "Write",
                serde_json::json!({}),
                CapabilityId::new("workspace.write").unwrap(),
                EffectClass::Write,
            ),
        ];

        let catalog = ToolPolicyCatalog::from_tools(&tools);
        assert!(!catalog.requires_approval("read_file"));
        assert!(catalog.requires_approval("write_file"));
        assert!(catalog.requires_approval("unknown_tool"));
        assert!(catalog.is_allowed("write_file"));
        assert!(!catalog.is_allowed("unknown_tool"));
    }
}
