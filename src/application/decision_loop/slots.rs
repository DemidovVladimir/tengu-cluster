//! Argument slots: turn a `SlotConfig` into labelled candidates the decision
//! model can choose from, and render the chosen values into tool arguments.
//!
//! History- and observation-sourced candidates get short labels (`pool_1`,
//! `pool_2`) so the model never has to reproduce an address; the full value
//! stays in code.

use std::collections::{BTreeMap, VecDeque};

use serde_json::Value;

use super::reduce::select;
use super::world::World;
use crate::config::decision_loop::SlotConfig;
use crate::domain::decision::HistoryEntry;

/// Max chars of an item shown to the model as a candidate description.
/// Longer items drop whole fields (`describe`); a value is never cut.
const MAX_DESC_CHARS: usize = 300;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Candidate {
    pub label: String,
    pub value: Value,
    pub description: String,
}

/// Candidates for one slot, capped by `cap` when set. Empty = the action is
/// not legal right now (e.g. the `from` action has not succeeded yet, or the
/// `observation` world entry is not fresh).
pub(crate) fn candidates(
    slot_name: &str,
    slot: &SlotConfig,
    cap: Option<f64>,
    history: &VecDeque<HistoryEntry>,
    world: &World,
) -> Vec<Candidate> {
    let within_cap = |v: &Value| match (cap, as_f64(v)) {
        (Some(c), Some(x)) => x <= c,
        (Some(_), None) => false,
        (None, _) => true,
    };
    let from_items = |root: &Value, items: &str, value: &str, top: usize| -> Vec<Candidate> {
        let Value::Array(list) = select(root, items) else {
            return Vec::new();
        };
        list.iter()
            .filter_map(|item| item.get(value).cloned().map(|v| (item, v)))
            .filter(|(_, v)| within_cap(v))
            .take(top)
            .enumerate()
            .map(|(i, (item, v))| Candidate {
                label: format!("{slot_name}_{}", i + 1),
                value: v,
                description: describe(item, value),
            })
            .collect()
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
            from_items(&entry.result, items, value, *top)
        }
        SlotConfig::FromObservation {
            observation,
            items,
            value,
            top,
        } => match world.fresh_root(observation) {
            Some(root) => from_items(&root, items, value, *top),
            None => Vec::new(),
        },
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

/// Candidate description: the item's JSON when it fits `MAX_DESC_CHARS`.
/// Otherwise whole `"key":value` fields in key order while they fit — the
/// `value` field always (it is what the label stands for) — and a trailing
/// `,…` marks the omission. Never cuts inside a value, so an address or
/// mint is shown whole or not at all.
fn describe(item: &Value, value: &str) -> String {
    let full = item.to_string();
    let Value::Object(o) = item else {
        return full;
    };
    if full.chars().count() <= MAX_DESC_CHARS {
        return full;
    }
    let field = |k: &String, v: &Value| format!("{}:{v}", Value::String(k.clone()));
    // `{` + the kept fields, each followed by `,` + `…}`.
    let mut used = "{…}".chars().count()
        + o.get_key_value(value)
            .map_or(0, |(k, v)| field(k, v).chars().count() + 1);
    let mut kept = String::from("{");
    for (k, v) in o {
        let f = field(k, v);
        let n = f.chars().count() + 1;
        if k == value || used + n <= MAX_DESC_CHARS {
            if k != value {
                used += n;
            }
            kept.push_str(&f);
            kept.push(',');
        }
    }
    kept + "…}"
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
            obs: None,
        }
    }

    #[test]
    fn static_candidates_respect_cap() {
        let slot = SlotConfig::Static(vec![json!(0.5), json!(1), json!(2), json!(3)]);
        let c = candidates("size", &slot, Some(2.0), &VecDeque::new(), &World::empty());
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
        let c = candidates("pool", &slot, None, &h, &World::empty());
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].label, "pool_1");
        assert_eq!(c[0].value, json!("A1"));
        assert!(c[0].description.contains("fees"));
    }

    #[test]
    fn long_descriptions_drop_whole_fields_never_cut_a_value() {
        // A full `dlmm_pools` row (keys serialise alphabetically): > 300 chars.
        let row = json!({
            "address": "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6",
            "apr_pct": 41.234567, "base_fee_pct": 0.04, "bin_step": 4,
            "current_price": 116.6084160651512, "dynamic_fee_pct": 0.001234,
            "fee_tvl_24h_pct": 0.112968, "fees_24h_usd": 7985.123456,
            "is_blacklisted": false,
            "mint_x": "So11111111111111111111111111111111111111112",
            "mint_y": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "name": "SOL-USDC", "tvl_usd": 7067545.74, "volume_24h_usd": 19963421.12
        });
        assert!(row.to_string().chars().count() > MAX_DESC_CHARS);
        // `value = "name"` sorts late: it must still show.
        let slot = SlotConfig::FromHistory {
            from: "fetch".into(),
            items: "/pools/*".into(),
            value: "name".into(),
            top: 1,
        };
        let h = VecDeque::from([entry("fetch", true, json!({ "pools": [row.clone()] }))]);
        let c = candidates("pool", &slot, None, &h, &World::empty());
        let d = &c[0].description;
        assert!(d.chars().count() <= MAX_DESC_CHARS, "{d}");
        assert!(d.contains(r#""name":"SOL-USDC""#), "{d}");
        assert!(d.contains(r#""address":"5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6""#));
        assert!(d.ends_with(",…}"), "omission marked: {d}");
        for (k, v) in row.as_object().unwrap() {
            let key = format!("{}:", json!(k));
            assert!(
                d.contains(&format!("{key}{v}")) || !d.contains(&key),
                "field `{k}` cut: {d}"
            );
        }
        // Short items are rendered whole, unchanged.
        let short = json!({"address": "A1", "fees": 5});
        let h = VecDeque::from([entry("fetch", true, json!({ "pools": [short.clone()] }))]);
        let c = candidates("pool", &slot_value("address"), None, &h, &World::empty());
        assert_eq!(c[0].description, short.to_string());
    }

    fn slot_value(value: &str) -> SlotConfig {
        SlotConfig::FromHistory {
            from: "fetch".into(),
            items: "/pools/*".into(),
            value: value.into(),
            top: 5,
        }
    }

    #[test]
    fn history_slot_empty_before_source_ran() {
        let slot = SlotConfig::FromHistory {
            from: "fetch".into(),
            items: "/pools/*".into(),
            value: "address".into(),
            top: 5,
        };
        assert!(candidates("pool", &slot, None, &VecDeque::new(), &World::empty()).is_empty());
    }

    #[test]
    fn observation_slot_empty_without_a_fresh_world_entry() {
        let slot = SlotConfig::FromObservation {
            observation: "pools".into(),
            items: "/data/pools/*".into(),
            value: "address".into(),
            top: 5,
        };
        assert!(candidates("pool", &slot, None, &VecDeque::new(), &World::empty()).is_empty());
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
