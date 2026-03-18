use anyhow::{bail, Result};
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use tengu_core::types::{ToolDef, ToolPolicyMetadata, ToolRiskLevel};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CapabilityId(String);

impl CapabilityId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_capability_id(&value)?;
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for CapabilityId {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::new(s).map_err(|e| e.to_string())
    }
}

fn validate_capability_id(value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("capability id cannot be empty");
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        bail!(
            "capability id '{}' must use lowercase letters, digits, dot, underscore, or hyphen",
            value
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffectClass {
    Read,
    Write,
    ExternalApi,
    ChainTx,
    ShellExec,
}

impl EffectClass {
    pub(crate) fn requires_approval(self) -> bool {
        !matches!(self, Self::Read)
    }

    pub(crate) fn risk_level(self) -> ToolRiskLevel {
        match self {
            Self::Read => ToolRiskLevel::Low,
            Self::Write => ToolRiskLevel::Medium,
            Self::ExternalApi => ToolRiskLevel::Medium,
            Self::ChainTx | Self::ShellExec => ToolRiskLevel::High,
        }
    }
}

impl FromStr for EffectClass {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "external_api" | "external-api" => Ok(Self::ExternalApi),
            "chain_tx" | "chain-tx" => Ok(Self::ChainTx),
            "shell_exec" | "shell-exec" => Ok(Self::ShellExec),
            other => Err(format!("unknown effect class '{}'", other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ToolClass {
    ReadTool,
    PrepareTool,
    ExecuteTool,
}

impl ToolClass {
    fn default_for_effect(effect_class: EffectClass) -> Self {
        match effect_class {
            EffectClass::Read => Self::ReadTool,
            EffectClass::Write
            | EffectClass::ExternalApi
            | EffectClass::ChainTx
            | EffectClass::ShellExec => Self::ExecuteTool,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ToolRuntimeMetadata {
    #[allow(dead_code)]
    pub tool_class: ToolClass,
    #[allow(dead_code)]
    pub output_schema: Option<serde_json::Value>,
    pub required_secrets: Vec<String>,
    #[allow(dead_code)]
    pub host_allowlist: Vec<String>,
    pub activity_description: Option<String>,
}

impl Default for ToolClass {
    fn default() -> Self {
        Self::ExecuteTool
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RegisteredTool {
    pub def: ToolDef,
    pub capability: CapabilityId,
    pub effect_class: EffectClass,
    pub metadata: ToolRuntimeMetadata,
}

impl RegisteredTool {
    pub(crate) fn new(
        name: &str,
        description: &str,
        parameters: serde_json::Value,
        capability: CapabilityId,
        effect_class: EffectClass,
    ) -> Self {
        Self {
            def: ToolDef {
                name: name.into(),
                description: description.into(),
                parameters,
                policy: Some(ToolPolicyMetadata {
                    risk_level: effect_class.risk_level(),
                    requires_approval: effect_class.requires_approval(),
                }),
            },
            capability,
            effect_class,
            metadata: ToolRuntimeMetadata {
                tool_class: ToolClass::default_for_effect(effect_class),
                ..ToolRuntimeMetadata::default()
            },
        }
    }

    #[allow(dead_code)]
    pub(crate) fn with_tool_class(mut self, tool_class: ToolClass) -> Self {
        self.metadata.tool_class = tool_class;
        self
    }

    #[allow(dead_code)]
    pub(crate) fn with_output_schema(mut self, output_schema: serde_json::Value) -> Self {
        self.metadata.output_schema = Some(output_schema);
        self
    }

    pub(crate) fn with_required_secrets(mut self, required_secrets: &[&str]) -> Self {
        self.metadata.required_secrets = required_secrets.iter().map(|s| s.to_string()).collect();
        self
    }

    #[allow(dead_code)]
    pub(crate) fn with_host_allowlist(mut self, host_allowlist: &[&str]) -> Self {
        self.metadata.host_allowlist = host_allowlist.iter().map(|s| s.to_string()).collect();
        self
    }

    pub(crate) fn with_activity_description(
        mut self,
        activity_description: impl Into<String>,
    ) -> Self {
        self.metadata.activity_description = Some(activity_description.into());
        self
    }
}

pub(crate) fn parse_capability_set(values: &[String]) -> Result<HashSet<CapabilityId>> {
    values
        .iter()
        .map(|value| CapabilityId::new(value.clone()))
        .collect()
}

pub(crate) fn filter_tools_by_capability(
    tools: Vec<RegisteredTool>,
    capabilities: &HashSet<CapabilityId>,
) -> Vec<RegisteredTool> {
    tools
        .into_iter()
        .filter(|tool| capabilities.contains(&tool.capability))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_id_rejects_invalid_values() {
        assert!("".parse::<CapabilityId>().is_err());
        assert!("Upper".parse::<CapabilityId>().is_err());
        assert!("bad space".parse::<CapabilityId>().is_err());
    }

    #[test]
    fn capability_id_accepts_expected_values() {
        let parsed = "skill.aura_orchestrator".parse::<CapabilityId>().unwrap();
        assert_eq!(parsed.as_str(), "skill.aura_orchestrator");
    }

    #[test]
    fn effect_class_maps_to_approval() {
        assert!(!EffectClass::Read.requires_approval());
        assert!(EffectClass::Write.requires_approval());
        assert!(EffectClass::ExternalApi.requires_approval());
        assert!(EffectClass::ChainTx.requires_approval());
        assert!(EffectClass::ShellExec.requires_approval());
    }

    #[test]
    fn tool_class_defaults_follow_effect_class() {
        let read_tool = RegisteredTool::new(
            "read_file",
            "Read",
            serde_json::json!({}),
            CapabilityId::new("workspace.read").unwrap(),
            EffectClass::Read,
        );
        let exec_tool = RegisteredTool::new(
            "write_file",
            "Write",
            serde_json::json!({}),
            CapabilityId::new("workspace.write").unwrap(),
            EffectClass::Write,
        );
        assert_eq!(read_tool.metadata.tool_class, ToolClass::ReadTool);
        assert_eq!(exec_tool.metadata.tool_class, ToolClass::ExecuteTool);
    }
}
