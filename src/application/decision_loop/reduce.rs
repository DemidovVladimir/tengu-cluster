//! Reducers: shrink raw tool results / events into the compact, normalised
//! shape the decision model reads (`state.history[].result`, `state.event`).
//!
//! Path grammar (JSON-pointer-like):
//! - `/`-separated segments; object key or array index
//! - `*` maps over an array (or an object's values); nested `*` flattens
//! - a final `{a,b,c}` projects those fields of each object
//!
//! `"/data/*/{address,fees_24h}"` on `{"data":[{"address":"A","fees_24h":1,"x":2}]}`
//! → `[{"address":"A","fees_24h":1}]`.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// Arrays in a reduced value are cut to this many items.
pub(crate) const MAX_ITEMS: usize = 20;
/// Unreduced string results are cut to this many chars.
pub(crate) const MAX_RAW_CHARS: usize = 2_000;

/// Apply `spec` (`name → path`) to `v`. Empty spec = `v` itself, trimmed.
pub(crate) fn reduce(v: &Value, spec: &BTreeMap<String, String>) -> Value {
    if spec.is_empty() {
        return trim(v.clone());
    }
    let mut out = Map::new();
    for (name, path) in spec {
        out.insert(name.clone(), trim(select(v, path)));
    }
    Value::Object(out)
}

/// Evaluate one path against `v`. Missing keys yield `Null`.
pub(crate) fn select(v: &Value, path: &str) -> Value {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    walk(v, &segs)
}

fn walk(v: &Value, segs: &[&str]) -> Value {
    let Some((seg, rest)) = segs.split_first() else {
        return v.clone();
    };
    if *seg == "*" {
        let items: Vec<&Value> = match v {
            Value::Array(a) => a.iter().collect(),
            Value::Object(o) => o.values().collect(),
            _ => return Value::Null,
        };
        let nested = rest.contains(&"*");
        let mut out = Vec::new();
        for item in items {
            match (nested, walk(item, rest)) {
                (true, Value::Array(inner)) => out.extend(inner),
                (_, Value::Null) => {}
                (_, x) => out.push(x),
            }
        }
        return Value::Array(out);
    }
    if let Some(fields) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
        let Value::Object(o) = v else {
            return Value::Null;
        };
        let mut out = Map::new();
        for f in fields.split(',').map(str::trim).filter(|f| !f.is_empty()) {
            if let Some(x) = o.get(f) {
                out.insert(f.to_string(), x.clone());
            }
        }
        return Value::Object(out);
    }
    let next = match v {
        Value::Object(o) => o.get(*seg),
        Value::Array(a) => seg.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    };
    next.map(|n| walk(n, rest)).unwrap_or(Value::Null)
}

fn trim(v: Value) -> Value {
    match v {
        Value::Array(mut a) => {
            a.truncate(MAX_ITEMS);
            Value::Array(a)
        }
        Value::String(s) if s.chars().count() > MAX_RAW_CHARS => {
            Value::String(s.chars().take(MAX_RAW_CHARS).collect::<String>() + "…")
        }
        other => other,
    }
}

/// The HTTP status of an `http_request` text result (line 1 `HTTP <status>
/// …`); `None` for any other text. A feed classifies it
/// (`runtime/feeds.rs`); [`http_ok`] is the pass / fail rule.
pub(crate) fn http_status(line1: &str) -> Option<u16> {
    line1
        .strip_prefix("HTTP ")
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse::<u16>().ok())
}

/// Whether an `http_request` text result succeeded (2xx); `None` for any
/// other text. A loop's `ok` ([`parse_tool_output`]) and the trace's
/// `tool.failed` (`trace_exec`) read it.
pub(crate) fn http_ok(line1: &str) -> Option<bool> {
    http_status(line1).map(|s| (200..300).contains(&s))
}

/// Turn a tool's text output into `(ok, value)`. `http_request` output
/// (`HTTP <status> <url>\n<body>`) → ok = 2xx, value = body parsed as JSON
/// when possible. Other text → JSON if it parses, else the string.
pub(crate) fn parse_tool_output(text: &str) -> (bool, Value) {
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    if let Some(ok) = http_ok(first) {
        let body = rest.trim();
        let value = if body.is_empty() {
            Value::String(first.to_string())
        } else {
            serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_string()))
        };
        return (ok, value);
    }
    let value = serde_json::from_str(text.trim()).unwrap_or_else(|_| Value::String(text.into()));
    (true, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_fields_over_array() {
        let v = json!({"data":[{"address":"A","fees_24h":1,"x":2},{"address":"B","fees_24h":3}]});
        assert_eq!(
            select(&v, "/data/*/{address,fees_24h}"),
            json!([{"address":"A","fees_24h":1},{"address":"B","fees_24h":3}])
        );
    }

    #[test]
    fn nested_wildcards_flatten() {
        let v = json!({"groups":[{"pairs":[{"a":1},{"a":2}]},{"pairs":[{"a":3}]}]});
        assert_eq!(select(&v, "/groups/*/pairs/*/a"), json!([1, 2, 3]));
    }

    #[test]
    fn index_and_missing() {
        let v = json!({"xs":[10, 20]});
        assert_eq!(select(&v, "/xs/1"), json!(20));
        assert_eq!(select(&v, "/nope/1"), Value::Null);
    }

    #[test]
    fn reduce_names_fields_and_truncates() {
        let many: Vec<i32> = (0..50).collect();
        let r = reduce(
            &json!({"xs": many}),
            &BTreeMap::from([("xs".into(), "/xs".into())]),
        );
        assert_eq!(r["xs"].as_array().unwrap().len(), MAX_ITEMS);
    }

    #[test]
    fn parses_http_request_output() {
        let (ok, v) = parse_tool_output("HTTP 200 https://x\n{\"a\":1}");
        assert!(ok);
        assert_eq!(v, json!({"a":1}));
        let (ok, v) = parse_tool_output("HTTP 404 https://x\nnot found");
        assert!(!ok);
        assert_eq!(v, json!("not found"));
        let (ok, _) = parse_tool_output("HTTP 200 https://x");
        assert!(ok);
    }
}
