//! Shared helpers for plugin `Tool::execute` JSON argument extraction.
//!
//! Centralises the repetitive `args.get(k).and_then(..).ok_or_else(..)`
//! pattern. Each helper returns `anyhow::Result<T>` with a consistent
//! error message shape: `"<tool>: '<key>' is required (<type>)"`.

use anyhow::{anyhow, Result};
use serde_json::Value;

/// Extract a required string argument. Returns an error keyed on `tool_name`
/// and `key` when the field is missing or not a string.
pub(crate) fn require_str<'a>(args: &'a Value, tool_name: &str, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{}: '{}' is required (string)", tool_name, key))
}

/// Extract a required i64 argument. JSON numbers round-trip through `as_i64`.
#[allow(dead_code)]
pub(crate) fn require_i64(args: &Value, tool_name: &str, key: &str) -> Result<i64> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow!("{}: '{}' is required (integer)", tool_name, key))
}

/// Extract a required boolean argument.
#[allow(dead_code)]
pub(crate) fn require_bool(args: &Value, tool_name: &str, key: &str) -> Result<bool> {
    args.get(key)
        .and_then(|v| v.as_bool())
        .ok_or_else(|| anyhow!("{}: '{}' is required (boolean)", tool_name, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn require_str_ok() {
        let v = json!({ "x": "hi" });
        assert_eq!(require_str(&v, "t", "x").unwrap(), "hi");
    }

    #[test]
    fn require_str_missing() {
        let v = json!({});
        let err = require_str(&v, "t", "x").unwrap_err().to_string();
        assert!(err.contains("t: 'x' is required"), "got: {}", err);
    }

    #[test]
    fn require_str_wrong_type() {
        let v = json!({ "x": 5 });
        assert!(require_str(&v, "t", "x").is_err());
    }

    #[test]
    fn require_i64_ok() {
        let v = json!({ "n": 7 });
        assert_eq!(require_i64(&v, "t", "n").unwrap(), 7);
    }

    #[test]
    fn require_bool_ok() {
        let v = json!({ "b": true });
        assert!(require_bool(&v, "t", "b").unwrap());
    }
}
