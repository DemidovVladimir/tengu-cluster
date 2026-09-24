//! Argument slots: turn a `SlotConfig` into labelled candidates the decision
//! model can choose from, and render the chosen values into tool arguments.
//!
//! History-sourced candidates get short labels (`pool_1`, `pool_2`) so the
//! model never has to reproduce an address; the full value stays in code.

use std::collections::{BTreeMap, VecDeque};

use serde_json::Value;

use super::reduce::select;
use crate::config::decision_loop::SlotConfig;
use crate::domain::decision::HistoryEntry;

/// Max chars of an item shown to the model as a candidate description.
const MAX_DESC_CHARS: usize = 300;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Candidate {
    pub label: String,
    pub value: Value,
    pub description: String,
}

/// Candidates for one slot, capped by `cap` when set. Empty = the action is
/// not legal right now (e.g. the `from` action has not succeeded yet).
pub(crate) fn candidates(
    slot_name: &str,
    slot: &SlotConfig,
    cap: Option<f64>,
    history: &VecDeque<HistoryEntry>,
) -> Vec<Candidate> {
    let within_cap = |v: &Value| match (cap, as_f64(v)) {
        (Some(c), Some(x)) => x <= c,
        (Some(_), None) => false,
        (None, _) => true,
    };
    match slot {
        SlotConfig::Static(values) => values
            .iter()
            .filter(|v| within_cap(v))
            .map(|v| {
                let label = plain(v);
                Candidate {
                    label: label.clone(),
                    value: v.clone(),
                    description: label,
                }
            })
            .collect(),
        SlotConfig::FromHistory {
            from,
            items,
            value,
            top,
        } => {
            let Some(entry) = history
                .iter()
                .rev()
                .find(|h| h.action == *from && h.ok == Some(true))
            else {
                return Vec::new();
            };
            let Value::Array(list) = select(&entry.result, items) else {
                return Vec::new();
            };
            list.iter()
                .filter_map(|item| item.get(value).cloned().map(|v| (item, v)))
                .filter(|(_, v)| within_cap(v))
                .take(*top)
                .enumerate()
                .map(|(i, (item, v))| Candidate {
                    label: format!("{slot_name}_{}", i + 1),
                    value: v,
                    description: truncate(&item.to_string()),
                })
                .collect()
        }
    }
}

/// Numeric view of a slot value (`2`, `2.5`, `"2"`).
pub(crate) fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Substitute `{slot}` references in `template`. A string that is exactly
/// `"{slot}"` becomes the slot value with its JSON type; otherwise the value
/// is spliced in as text.
pub(crate) fn render_args(template: &Value, values: &BTreeMap<String, Value>) -> Value {
    match template {
        Value::String(s) => {
            if let Some(name) = s.strip_prefix('{').and_then(|x| x.strip_suffix('}')) {
                if let Some(v) = values.get(name) {
                    return v.clone();
                }
            }
            let mut out = s.clone();
            for (k, v) in values {
                out = out.replace(&format!("{{{k}}}"), &plain(v));
            }
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| render_args(x, values)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), render_args(x, values)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_DESC_CHARS {
        return s.to_string();
    }
    s.chars().take(MAX_DESC_CHARS).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(action: &str, ok: bool, result: Value) -> HistoryEntry {
        HistoryEntry {
            t: 1,
            action: action.into(),
            args: Value::Null,
            ok: Some(ok),
            result,
        }
    }

    #[test]
    fn static_candidates_respect_cap() {
        let slot = SlotConfig::Static(vec![json!(0.5), json!(1), json!(2), json!(3)]);
        let c = candidates("size", &slot, Some(2.0), &VecDeque::new());
        let labels: Vec<_> = c.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["0.5", "1", "2"]);
    }

    #[test]
    fn history_candidates_use_latest_success_and_short_labels() {
        let slot = SlotConfig::FromHistory {
            from: "fetch".into(),
            items: "/pools/*".into(),
            value: "address".into(),
            top: 2,
        };
        let mut h = VecDeque::new();
        h.push_back(entry("fetch", true, json!({"pools":[{"address":"OLD"}]})));
        h.push_back(entry(
            "fetch",
            true,
            json!({"pools":[{"address":"A1","fees":5},{"address":"B2"},{"address":"C3"}]}),
        ));
        h.push_back(entry("fetch", false, json!("HTTP 500")));
        let c = candidates("pool", &slot, None, &h);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].label, "pool_1");
        assert_eq!(c[0].value, json!("A1"));
        assert!(c[0].description.contains("fees"));
    }

    #[test]
    fn history_slot_empty_before_source_ran() {
        let slot = SlotConfig::FromHistory {
            from: "fetch".into(),
            items: "/pools/*".into(),
            value: "address".into(),
            top: 5,
        };
        assert!(candidates("pool", &slot, None, &VecDeque::new()).is_empty());
    }

    #[test]
    fn render_keeps_types_for_whole_slot_strings() {
        let t = json!({"url": "https://x/{pair}?n={n}", "amount": "{n}", "nested": ["{pair}"]});
        let v = BTreeMap::from([("pair".into(), json!("SOL-USDC")), ("n".into(), json!(2))]);
        assert_eq!(
            render_args(&t, &v),
            json!({"url": "https://x/SOL-USDC?n=2", "amount": 2, "nested": ["SOL-USDC"]})
        );
    }
}
