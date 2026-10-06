//! Argument slots: turn a `SlotConfig` into labelled candidates the decision
//! model can choose from, and render the chosen values into tool arguments.
//!
//! History-, observation- and event-sourced list candidates get short labels
//! (`pool_1`, `pool_2`) so the model never has to reproduce an address; the
//! full value stays in code. A bound slot (`path`, or `event` without
//! `value`) is one candidate — filled without a question — or none, which
//! keeps the action illegal.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde_json::Value;

use super::reduce::select;
use super::world::World;
use crate::config::decision_loop::{SlotConfig, SlotMode};
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
/// not legal right now (e.g. the `from` action has not succeeded yet in this
/// event, the `observation` world entry is not fresh, or a bound path reads
/// nothing). `history` = the current event's entries, oldest first: the loop
/// never offers items from an earlier event's result (its data may be hours
/// old). `event` = the reduced incoming event.
pub(crate) fn candidates<'h>(
    slot_name: &str,
    slot: &SlotConfig,
    cap: Option<f64>,
    history: impl DoubleEndedIterator<Item = &'h HistoryEntry>,
    world: &World,
    event: &Value,
) -> Vec<Candidate> {
    let within_cap = |v: &Value| match (cap, as_f64(v)) {
        (Some(c), Some(x)) => x <= c,
        (Some(_), None) => false,
        (None, _) => true,
    };
    let root: Cow<'_, Value> = match slot {
        SlotConfig::Static(values) => {
            return values
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
                .collect()
        }
        SlotConfig::FromHistory { from, .. } => {
            match history
                .rev()
                .find(|h| h.action == *from && h.ok == Some(true))
            {
                Some(entry) => Cow::Borrowed(&entry.result),
                None => return Vec::new(),
            }
        }
        SlotConfig::FromObservation { observation, .. } => match world.fresh_root(observation) {
            Some(root) => Cow::Owned(root),
            None => return Vec::new(),
        },
        SlotConfig::FromEvent { .. } => Cow::Borrowed(event),
    };
    match slot.mode() {
        Some(SlotMode::List { items, value, top }) => {
            let Value::Array(list) = select(&root, items) else {
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
        }
        // One bound value: a single candidate, filled without a question.
        // Nothing there (`null`, an empty list) ⇒ no candidate — a binding
        // never invents a value.
        Some(SlotMode::One { path }) => {
            let v = select(&root, path);
            let empty = v.is_null() || v.as_array().is_some_and(Vec::is_empty);
            if empty || !within_cap(&v) {
                return Vec::new();
            }
            let label = plain(&v);
            vec![Candidate {
                label: label.clone(),
                value: v,
                description: label,
            }]
        }
        // Malformed (config validation refuses it at load).
        None => Vec::new(),
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
        let c = candidates(
            "size",
            &slot,
            Some(2.0),
            std::iter::empty(),
            &World::empty(),
            &Value::Null,
        );
        let labels: Vec<_> = c.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["0.5", "1", "2"]);
    }

    #[test]
    fn history_candidates_use_latest_success_and_short_labels() {
        let slot = SlotConfig::FromHistory {
            from: "fetch".into(),
            items: Some("/pools/*".into()),
            path: None,
            value: Some("address".into()),
            top: 2,
        };
        let mut h = Vec::new();
        h.push(entry("fetch", true, json!({"pools":[{"address":"OLD"}]})));
        h.push(entry(
            "fetch",
            true,
            json!({"pools":[{"address":"A1","fees":5},{"address":"B2"},{"address":"C3"}]}),
        ));
        h.push(entry("fetch", false, json!("HTTP 500")));
        let c = candidates("pool", &slot, None, h.iter(), &World::empty(), &Value::Null);
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
            items: Some("/pools/*".into()),
            path: None,
            value: Some("name".into()),
            top: 1,
        };
        let h = vec![entry("fetch", true, json!({ "pools": [row.clone()] }))];
        let c = candidates("pool", &slot, None, h.iter(), &World::empty(), &Value::Null);
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
        let h = vec![entry("fetch", true, json!({ "pools": [short.clone()] }))];
        let c = candidates(
            "pool",
            &slot_value("address"),
            None,
            h.iter(),
            &World::empty(),
            &Value::Null,
        );
        assert_eq!(c[0].description, short.to_string());
    }

    fn slot_value(value: &str) -> SlotConfig {
        SlotConfig::FromHistory {
            from: "fetch".into(),
            items: Some("/pools/*".into()),
            path: None,
            value: Some(value.into()),
            top: 5,
        }
    }

    #[test]
    fn history_slot_empty_before_source_ran() {
        let slot = SlotConfig::FromHistory {
            from: "fetch".into(),
            items: Some("/pools/*".into()),
            path: None,
            value: Some("address".into()),
            top: 5,
        };
        assert!(candidates(
            "pool",
            &slot,
            None,
            std::iter::empty(),
            &World::empty(),
            &Value::Null
        )
        .is_empty());
    }

    #[test]
    fn observation_slot_empty_without_a_fresh_world_entry() {
        let slot = SlotConfig::FromObservation {
            observation: "pools".into(),
            items: Some("/data/pools/*".into()),
            path: None,
            value: Some("address".into()),
            top: 5,
        };
        assert!(candidates(
            "pool",
            &slot,
            None,
            std::iter::empty(),
            &World::empty(),
            &Value::Null
        )
        .is_empty());
    }

    fn bound(path: &str) -> SlotConfig {
        SlotConfig::FromHistory {
            from: "plan".into(),
            items: None,
            value: None,
            path: Some(path.into()),
            top: 5,
        }
    }

    #[test]
    fn a_binding_is_one_candidate_or_none() {
        let plan = json!({"swaps": [{"amount": 81.6, "input_mint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"}], "none": null, "empty": []});
        let h = vec![entry("plan", true, plan)];
        let one = |slot: &SlotConfig, cap| {
            candidates("x", slot, cap, h.iter(), &World::empty(), &Value::Null)
        };
        let c = one(&bound("/swaps/0/input_mint"), None);
        assert_eq!(c.len(), 1);
        assert_eq!(
            c[0].value,
            json!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
        );
        assert_eq!(one(&bound("/swaps/0/amount"), None)[0].value, json!(81.6));
        // Caps hold for bound numbers too.
        assert!(one(&bound("/swaps/0/amount"), Some(50.0)).is_empty());
        // Nothing there is no candidate: missing, null, an empty list.
        for path in ["/swaps/1/amount", "/none", "/empty", "/nope"] {
            assert!(one(&bound(path), None).is_empty(), "{path}");
        }
        // A failed source action binds nothing.
        let failed = vec![entry("plan", false, json!({"swaps": [{"amount": 1}]}))];
        let slot = bound("/swaps/0/amount");
        assert!(candidates(
            "x",
            &slot,
            None,
            failed.iter(),
            &World::empty(),
            &Value::Null
        )
        .is_empty());
    }

    #[test]
    fn event_slots_bind_one_value_or_list_items() {
        let event = json!({"target_sol": 1.25, "pools": [{"address": "A1"}, {"address": "B2"}]});
        let one = SlotConfig::FromEvent {
            event: "/target_sol".into(),
            value: None,
            top: 5,
        };
        let c = candidates(
            "t",
            &one,
            Some(2.0),
            std::iter::empty(),
            &World::empty(),
            &event,
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].value, json!(1.25));
        let list = SlotConfig::FromEvent {
            event: "/pools/*".into(),
            value: Some("address".into()),
            top: 5,
        };
        let c = candidates(
            "pool",
            &list,
            None,
            std::iter::empty(),
            &World::empty(),
            &event,
        );
        let labels: Vec<_> = c.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["pool_1", "pool_2"]);
        assert_eq!(c[1].value, json!("B2"));
        // No event value ⇒ no candidate.
        assert!(candidates(
            "t",
            &one,
            None,
            std::iter::empty(),
            &World::empty(),
            &json!({})
        )
        .is_empty());
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
