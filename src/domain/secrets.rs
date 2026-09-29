//! `SecretRegistry` — secret values to redact from tool output, transcripts
//! and memory. Pure in-memory; the encrypted vault file that feeds it lives
//! in `adapters/outbound/secrets.rs`.

use std::collections::HashSet;

use serde_json::Value;

use crate::domain::observation::Observation;

/// Registry of secret values that must never appear in tool output,
/// flow transcripts, or memory storage.
pub(crate) struct SecretRegistry {
    /// Sorted longest-first to avoid partial-match issues during redaction.
    values: Vec<String>,
    seen: HashSet<String>,
}

impl SecretRegistry {
    pub fn new() -> Self {
        Self {
            values: Vec::new(),
            seen: HashSet::new(),
        }
    }

    /// Register a secret value for redaction. Empty or duplicate values are ignored.
    pub fn register(&mut self, value: String) {
        if value.is_empty() || self.seen.contains(&value) {
            return;
        }
        self.seen.insert(value.clone());
        self.values.push(value);
        // Re-sort longest-first so longer secrets are replaced before
        // any shorter substring match.
        self.values.sort_by(|a, b| b.len().cmp(&a.len()));
    }

    /// Replace every occurrence of any registered secret value with `[REDACTED]`.
    pub fn redact(&self, text: &str) -> String {
        if self.values.is_empty() {
            return text.to_string();
        }
        let mut result = text.to_string();
        for secret in &self.values {
            result = result.replace(secret.as_str(), "[REDACTED]");
        }
        result
    }

    /// `redact` every string leaf of `v` in place.
    pub fn redact_value(&self, v: &mut Value) {
        if self.values.is_empty() {
            return;
        }
        match v {
            Value::String(s) => {
                if self.values.iter().any(|x| s.contains(x.as_str())) {
                    *s = self.redact(s);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| self.redact_value(x)),
            Value::Object(o) => o.values_mut().for_each(|x| self.redact_value(x)),
            _ => {}
        }
    }

    /// Redact a typed observation: headline, `errors[].message`, string
    /// features and the whole `data` payload. The cache key is left alone.
    pub fn redact_observation(&self, obs: &mut Observation) {
        if self.values.is_empty() {
            return;
        }
        obs.headline = self.redact(&obs.headline);
        for e in &mut obs.errors {
            e.message = self.redact(&e.message);
        }
        obs.features.values_mut().for_each(|v| self.redact_value(v));
        self.redact_value(&mut obs.data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::observation::{ErrorClass, ObsSource, ObsStatus, ReadError};
    use serde_json::json;

    #[test]
    fn redact_value_walks_string_leaves() {
        let mut r = SecretRegistry::new();
        r.register("sk-secret".into());
        let mut v = json!({"a": ["x sk-secret y", 1], "b": {"c": "sk-secret"}});
        r.redact_value(&mut v);
        assert_eq!(
            v,
            json!({"a": ["x [REDACTED] y", 1], "b": {"c": "[REDACTED]"}})
        );
    }

    #[test]
    fn redact_observation_covers_every_text_field() {
        let mut r = SecretRegistry::new();
        r.register("sk-secret".into());
        let mut obs = Observation {
            key: "k/1:s".into(),
            schema: "k/1".into(),
            tool: "t".into(),
            observed_at_ms: 0,
            slot: None,
            ttl_ms: 1,
            source: ObsSource::Live,
            status: ObsStatus::Partial,
            errors: vec![ReadError::new("f", ErrorClass::Fatal, "bad sk-secret")],
            headline: "h sk-secret".into(),
            features: [("s".to_string(), json!("sk-secret"))].into(),
            data: json!({"url": "https://rpc/?api-key=sk-secret"}),
        };
        r.redact_observation(&mut obs);
        let all = serde_json::to_string(&obs).unwrap();
        assert!(!all.contains("sk-secret"), "{all}");
    }
}
