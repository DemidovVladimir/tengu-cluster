use crate::domain::tool_result::ToolResultEnvelope;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub(crate) struct TaskExecutionRecord {
    pub role: String,
    pub success: bool,
    pub output_summary: String,
    pub artifact_count: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RunState {
    task_outputs: HashMap<String, String>,
    task_records: HashMap<String, TaskExecutionRecord>,
    artifacts: BTreeMap<String, serde_json::Value>,
    stop_reason: Option<String>,
}

impl RunState {
    pub(crate) fn set_task_output(
        &mut self,
        task_id: impl Into<String>,
        output: impl Into<String>,
    ) {
        self.task_outputs.insert(task_id.into(), output.into());
    }

    pub(crate) fn set_task_record(
        &mut self,
        task_id: impl Into<String>,
        record: TaskExecutionRecord,
    ) {
        self.task_records.insert(task_id.into(), record);
    }

    pub(crate) fn ingest_tool_envelope(&mut self, envelope: &ToolResultEnvelope) {
        for (key, value) in &envelope.artifacts {
            self.artifacts.insert(key.clone(), value.clone());
        }
        for (key, value) in &envelope.ids {
            self.artifacts
                .insert(key.clone(), serde_json::Value::String(value.clone()));
        }
        for (key, value) in &envelope.urls {
            self.artifacts
                .insert(key.clone(), serde_json::Value::String(value.clone()));
        }
        for (key, value) in &envelope.hashes {
            self.artifacts
                .insert(key.clone(), serde_json::Value::String(value.clone()));
        }
    }

    pub(crate) fn has_artifact(&self, key: &str) -> bool {
        self.artifacts.contains_key(key)
    }

    pub(crate) fn missing_artifacts<'a>(&self, required: &'a [&'a str]) -> Vec<&'a str> {
        required
            .iter()
            .copied()
            .filter(|key| !self.has_artifact(key))
            .collect()
    }

    #[allow(dead_code)]
    pub(crate) fn record_for(&self, task_id: &str) -> Option<&TaskExecutionRecord> {
        self.task_records.get(task_id)
    }

    pub(crate) fn artifacts(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.artifacts
    }

    pub(crate) fn artifacts_for_prompt(&self, max_items: usize) -> String {
        if self.artifacts.is_empty() {
            return "No structured artifacts recorded yet.".to_string();
        }
        let mut lines = Vec::new();
        for (idx, (key, value)) in self.artifacts.iter().enumerate() {
            if idx >= max_items {
                break;
            }
            let rendered = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            lines.push(format!("- {} = {}", key, rendered));
        }
        lines.join("\n")
    }

    #[allow(dead_code)]
    pub(crate) fn set_stop_reason(&mut self, reason: impl Into<String>) {
        self.stop_reason = Some(reason.into());
    }

    #[allow(dead_code)]
    pub(crate) fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_tool_envelope_populates_artifacts() {
        let env = ToolResultEnvelope::ok("mint_ipnft", "Minted")
            .with_id("mint.token_id", "42")
            .with_url("mint.project_url", "https://example.test/ipnfts/42");
        let mut state = RunState::default();
        state.ingest_tool_envelope(&env);
        assert!(state.has_artifact("mint.token_id"));
        assert!(state.has_artifact("mint.project_url"));
    }
}
