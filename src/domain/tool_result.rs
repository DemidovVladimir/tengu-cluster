use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolResultStatus {
    #[default]
    Ok,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) struct ToolProvenance {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub workspace_paths: Vec<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ToolResultEnvelope {
    pub tool_name: String,
    #[serde(default)]
    pub status: ToolResultStatus,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub artifacts: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub ids: BTreeMap<String, String>,
    #[serde(default)]
    pub urls: BTreeMap<String, String>,
    #[serde(default)]
    pub hashes: BTreeMap<String, String>,
    #[serde(default)]
    pub raw_response: Option<serde_json::Value>,
    #[serde(default)]
    pub provenance: ToolProvenance,
}

impl ToolResultEnvelope {
    pub(crate) fn ok(tool_name: &str, summary: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            status: ToolResultStatus::Ok,
            summary: summary.into(),
            ..Self::default()
        }
    }

    #[allow(dead_code)]
    pub(crate) fn error(tool_name: &str, summary: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            status: ToolResultStatus::Error,
            summary: summary.into(),
            ..Self::default()
        }
    }

    pub(crate) fn with_artifact(
        mut self,
        key: impl Into<String>,
        value: impl Serialize,
    ) -> anyhow::Result<Self> {
        self.artifacts
            .insert(key.into(), serde_json::to_value(value)?);
        Ok(self)
    }

    pub(crate) fn with_id(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.ids.insert(key.into(), value.into());
        self
    }

    pub(crate) fn with_url(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.urls.insert(key.into(), value.into());
        self
    }

    pub(crate) fn with_hash(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.hashes.insert(key.into(), value.into());
        self
    }

    pub(crate) fn with_raw_response(mut self, value: serde_json::Value) -> Self {
        self.raw_response = Some(value);
        self
    }

    pub(crate) fn with_provenance(
        mut self,
        source: impl Into<String>,
        workspace_paths: Vec<String>,
        hosts: Vec<String>,
    ) -> Self {
        self.provenance = ToolProvenance {
            source: source.into(),
            workspace_paths,
            hosts,
        };
        self
    }

    pub(crate) fn to_json_string(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

pub(crate) fn parse_tool_result_envelope(text: &str) -> Option<ToolResultEnvelope> {
    serde_json::from_str(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_tool_result_envelope() {
        let env = ToolResultEnvelope::ok("mint_ipnft", "Minted IP-NFT")
            .with_id("mint.token_id", "42")
            .with_url("mint.project_url", "https://example.test/ipnfts/42");
        let json = env.to_json_string().unwrap();
        let parsed = parse_tool_result_envelope(&json).unwrap();
        assert_eq!(parsed.tool_name, "mint_ipnft");
        assert_eq!(parsed.ids["mint.token_id"], "42");
    }
}
