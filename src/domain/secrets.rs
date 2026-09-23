//! `SecretRegistry` — secret values to redact from tool output, transcripts
//! and memory. Pure in-memory; the encrypted vault file that feeds it lives
//! in `adapters/outbound/secrets.rs`.

use std::collections::HashSet;

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
}
