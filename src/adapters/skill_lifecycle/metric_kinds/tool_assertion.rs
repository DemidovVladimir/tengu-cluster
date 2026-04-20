//! `tool_assertion` — dispatch a registered workspace tool and assert on its output.

use anyhow::{bail, Result};
use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

use crate::adapters::skill_lifecycle::metrics::{
    FixtureContext, MetricKind, MetricOutcome, MetricRunCtx, MetricSpec,
};

pub(crate) struct ToolAssertionKind;

#[async_trait]
impl MetricKind for ToolAssertionKind {
    async fn run(
        &self,
        spec: &MetricSpec,
        _fixture: &FixtureContext<'_>,
        ctx: &MetricRunCtx<'_>,
    ) -> Result<MetricOutcome> {
        let (tool_name, action, key, assertion) = match spec {
            MetricSpec::ToolAssertion { tool, action, key, assert, .. } => {
                (tool.clone(), action.clone(), key.clone(), assert.clone())
            }
            _ => bail!("ToolAssertionKind given wrong spec"),
        };

        let Some(registry) = ctx.tools else {
            return Ok(MetricOutcome {
                pass: false,
                score: 0.0,
                notes: Some("tool registry unavailable in this run context".into()),
                raw: json!({}),
            });
        };

        // Arg shape is kind-specific: `{action, key?}` passed through to the tool.
        let mut args = json!({ "action": action });
        if let Some(k) = key {
            args["key"] = Value::String(k);
        }

        // Tool dispatch happens through the registry; a ToolCtx is required to call
        // `invoke`. The eval runner constructs one and passes it via a richer
        // MetricRunCtx extension in Task 12. For now, feature-gate: if the tool
        // isn't registered we fail; otherwise we pass when the assertion is
        // trivially satisfied against {"registered": true}.
        let registered = registry.definitions().iter().any(|d| d.name == tool_name);
        if !registered {
            return Ok(MetricOutcome {
                pass: false,
                score: 0.0,
                notes: Some(format!("tool '{tool_name}' not registered")),
                raw: json!({ "args": args }),
            });
        }

        // NOTE: actual async invoke happens via a dispatcher added in Task 12.
        // Until then, tool_assertion passes when the tool is registered AND the
        // assertion block is trivially satisfied against {"registered": true}.
        let pass = assert_value(&assertion, &json!({ "registered": true }));
        Ok(MetricOutcome {
            pass,
            score: if pass { 1.0 } else { 0.0 },
            notes: if pass { None } else { Some("assertion failed".into()) },
            raw: json!({ "args": args, "observed": { "registered": true } }),
        })
    }
}

fn assert_value(assertion: &Value, observed: &Value) -> bool {
    let Some(obj) = assertion.as_object() else { return false };
    for (k, v) in obj {
        match k.as_str() {
            "value_matches" => {
                let Some(pat) = v.as_str() else { return false };
                let Some(s) = observed_as_str(observed) else { return false };
                let Ok(re) = Regex::new(pat) else { return false };
                if !re.is_match(s) { return false; }
            }
            "value_equals" => {
                if observed != v { return false; }
            }
            "value_in" => {
                let Some(arr) = v.as_array() else { return false };
                if !arr.iter().any(|c| c == observed) { return false; }
            }
            _ => return false,
        }
    }
    true
}

fn observed_as_str(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s),
        Value::Object(m) => m.get("value").and_then(|x| x.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assert_value_matches_regex_on_string() {
        assert!(assert_value(
            &json!({"value_matches": "^0x[a-f0-9]+$"}),
            &json!("0xabc"),
        ));
        assert!(!assert_value(
            &json!({"value_matches": "^0x[a-f0-9]+$"}),
            &json!("nope"),
        ));
    }

    #[test]
    fn assert_value_equals() {
        assert!(assert_value(&json!({"value_equals": 42}), &json!(42)));
        assert!(!assert_value(&json!({"value_equals": 42}), &json!(43)));
    }

    #[test]
    fn assert_value_in() {
        assert!(assert_value(&json!({"value_in": ["a", "b"]}), &json!("a")));
        assert!(!assert_value(&json!({"value_in": ["a", "b"]}), &json!("c")));
    }
}
