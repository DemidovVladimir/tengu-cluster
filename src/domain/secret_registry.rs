//! Secret value registry for redacting sensitive data from output.
//!
//! Pure domain module — no I/O, no infrastructure dependencies.
//! Holds the set of known secret values and replaces any occurrence
//! in arbitrary text with `[REDACTED]`.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_is_noop() {
        let reg = SecretRegistry::new();
        assert_eq!(reg.redact("hello world"), "hello world");
    }

    #[test]
    fn redacts_single_secret() {
        let mut reg = SecretRegistry::new();
        reg.register("super_secret".to_string());
        assert_eq!(
            reg.redact("the key is super_secret ok"),
            "the key is [REDACTED] ok"
        );
    }

    #[test]
    fn redacts_multiple_secrets() {
        let mut reg = SecretRegistry::new();
        reg.register("aaa".to_string());
        reg.register("bbb".to_string());
        assert_eq!(reg.redact("aaa and bbb"), "[REDACTED] and [REDACTED]");
    }

    #[test]
    fn longer_secret_redacted_first() {
        let mut reg = SecretRegistry::new();
        reg.register("sk-".to_string());
        reg.register("sk-or-v1-longkey".to_string());
        let text = "key=sk-or-v1-longkey";
        let result = reg.redact(text);
        // The longer match should be replaced as a whole, not partially.
        assert_eq!(result, "key=[REDACTED]");
    }

    #[test]
    fn ignores_empty_values() {
        let mut reg = SecretRegistry::new();
        reg.register(String::new());
        assert_eq!(reg.redact("hello"), "hello");
    }

    #[test]
    fn deduplicates_registrations() {
        let mut reg = SecretRegistry::new();
        reg.register("secret".to_string());
        reg.register("secret".to_string());
        assert_eq!(reg.values.len(), 1);
    }

    #[test]
    fn redacts_multiple_occurrences() {
        let mut reg = SecretRegistry::new();
        reg.register("tok".to_string());
        assert_eq!(
            reg.redact("tok tok tok"),
            "[REDACTED] [REDACTED] [REDACTED]"
        );
    }
}
