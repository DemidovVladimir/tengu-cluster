//! Canonical JSON — one text per value, whatever the map insertion order —
//! and its sha256: the identity of a strategy spec (a backtest run's
//! `spec_sha256`, `application/backtest/`) and of a decisions request (the
//! decision cache key, `outbound/decision_cache.rs`).
//!
//! | Rule | Value |
//! |---|---|
//! | Objects | keys sorted (bytewise) at every depth, escaped as JSON strings |
//! | Arrays | order kept |
//! | Scalars | as `serde_json` prints them (`1`, `1.5`, `"é"`, `null`) |
//! | Hash | sha256 of the canonical text, 64 lowercase hex chars — never shortened |

use serde_json::Value;
use sha2::{Digest, Sha256};

/// `v` as JSON text with object keys sorted at every depth; arrays keep
/// their order, scalars print as serde_json prints them.
pub(crate) fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write_canonical(&o[k.as_str()], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(x, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// sha256 of `text`, 64 lowercase hex chars.
pub(crate) fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// sha256 hex of `v`'s canonical JSON.
pub(crate) fn canonical_sha256(v: &Value) -> String {
    sha256_hex(&canonical_json(v))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn keys_sort_at_every_depth_and_arrays_keep_their_order() {
        assert_eq!(
            canonical_json(&json!({"z": [3, 1], "a\"b": {"y": 1.5, "x": "é"}})),
            r#"{"a\"b":{"x":"é","y":1.5},"z":[3,1]}"#
        );
        let mut a = serde_json::Map::new();
        a.insert("b".into(), json!({"d": 1, "c": [true, null]}));
        a.insert("a".into(), json!(0));
        let mut b = serde_json::Map::new();
        b.insert("a".into(), json!(0));
        b.insert("b".into(), json!({"c": [true, null], "d": 1}));
        let (a, b) = (Value::Object(a), Value::Object(b));
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(canonical_sha256(&a), canonical_sha256(&b));
    }

    #[test]
    fn the_hash_is_64_hex_chars() {
        // sha256("") — the well-known empty digest.
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let h = canonical_sha256(&json!({"kind": "weekend_window"}));
        assert_eq!(h.len(), 64);
        assert!(h
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
        assert_ne!(h, canonical_sha256(&json!({"kind": "daily_window"})));
    }
}
