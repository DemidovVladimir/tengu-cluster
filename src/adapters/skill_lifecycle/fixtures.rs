//! `evals/prompts.yaml` read/write + mechanical transcript→fixture extraction.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::adapters::types::{Message, Role, ToolCall};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct FixturesFile {
    pub schema_version: u32,
    pub fixtures: Vec<Fixture>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Fixture {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub expected_tool_calls: Vec<ExpectedToolCall>,
    #[serde(default)]
    pub expected_outcome: Option<String>,
    #[serde(default)]
    pub metrics: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct ExpectedToolCall {
    pub tool: String,
    pub args_schema: serde_json::Value,
}

pub(crate) fn read_fixtures(path: &Path) -> Result<FixturesFile> {
    let body = std::fs::read_to_string(path)?;
    let parsed: FixturesFile = serde_yaml::from_str(&body)?;
    if parsed.schema_version != 1 {
        bail!(
            "unsupported fixtures schema_version: {}",
            parsed.schema_version
        );
    }
    Ok(parsed)
}

pub(crate) fn write_fixtures(path: &Path, file: &FixturesFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_yaml::to_string(file)?)?;
    Ok(())
}

pub(crate) struct ExtractOpts {
    pub include_user_messages: bool,
    pub expected_outcome: Option<String>,
    pub drop_tool_names: Vec<String>,
    pub metric_names: Vec<String>,
}

/// Mechanically extract fixtures from a message slice. Never calls an LLM.
/// Pairs are (user, assistant) in order; long string args are schema-redacted.
pub(crate) fn extract_fixtures(messages: &[Message], opts: &ExtractOpts) -> Vec<Fixture> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    let mut fixture_n = 1;

    while idx < messages.len() {
        // Find next user message
        while idx < messages.len() && !matches!(messages[idx].role, Role::User) {
            idx += 1;
        }
        if idx >= messages.len() {
            break;
        }
        let user = &messages[idx];
        idx += 1;

        // Find next assistant message (skip tool messages interleaved)
        let mut asst_idx = idx;
        while asst_idx < messages.len() && !matches!(messages[asst_idx].role, Role::Assistant) {
            asst_idx += 1;
        }
        if asst_idx >= messages.len() {
            break;
        }
        let asst = &messages[asst_idx];
        idx = asst_idx + 1;

        let expected_tool_calls = extract_tool_calls(&asst.tool_calls, &opts.drop_tool_names);

        out.push(Fixture {
            id: format!("f{}", fixture_n),
            prompt: if opts.include_user_messages {
                user.content.clone()
            } else {
                String::new()
            },
            expected_tool_calls,
            expected_outcome: opts.expected_outcome.clone(),
            metrics: opts.metric_names.clone(),
        });
        fixture_n += 1;
    }
    out
}

fn extract_tool_calls(calls: &Option<Vec<ToolCall>>, drop: &[String]) -> Vec<ExpectedToolCall> {
    let Some(calls) = calls else {
        return Vec::new();
    };
    calls
        .iter()
        .filter(|c| !drop.iter().any(|d| d == &c.name))
        .map(|c| ExpectedToolCall {
            tool: c.name.clone(),
            args_schema: redact_args(&c.arguments),
        })
        .collect()
}

fn redact_args(args: &serde_json::Value) -> serde_json::Value {
    match args {
        serde_json::Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, v) in m {
                out.insert(k.clone(), redact_args(v));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(redact_args).collect())
        }
        serde_json::Value::String(s) if s.len() > 32 => {
            serde_json::Value::String("<elided>".into())
        }
        _ => args.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn asst_with_calls(calls: Vec<ToolCall>) -> Message {
        Message {
            role: Role::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_calls: Some(calls),
        }
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: args,
        }
    }

    #[test]
    fn yaml_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("p.yaml");
        let f = FixturesFile {
            schema_version: 1,
            fixtures: vec![Fixture {
                id: "f1".into(),
                prompt: "hi".into(),
                expected_tool_calls: vec![],
                expected_outcome: Some("ok".into()),
                metrics: vec!["m1".into()],
            }],
        };
        write_fixtures(&path, &f).unwrap();
        let back = read_fixtures(&path).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn schema_version_mismatch_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("p.yaml");
        std::fs::write(&path, "schema_version: 99\nfixtures: []\n").unwrap();
        assert!(read_fixtures(&path).is_err());
    }

    #[test]
    fn extract_pairs_user_and_assistant() {
        let messages = vec![
            msg(Role::User, "req1"),
            msg(Role::Assistant, "resp1"),
            msg(Role::User, "req2"),
            msg(Role::Assistant, "resp2"),
        ];
        let opts = ExtractOpts {
            include_user_messages: true,
            expected_outcome: None,
            drop_tool_names: vec![],
            metric_names: vec!["m1".into()],
        };
        let fxs = extract_fixtures(&messages, &opts);
        assert_eq!(fxs.len(), 2);
        assert_eq!(fxs[0].prompt, "req1");
        assert_eq!(fxs[1].id, "f2");
        assert_eq!(fxs[0].metrics, vec!["m1".to_string()]);
    }

    #[test]
    fn extract_redacts_long_string_args() {
        let long = "x".repeat(50);
        let messages = vec![
            msg(Role::User, "req"),
            asst_with_calls(vec![call(
                "http_request",
                json!({
                    "url": long.clone(),
                    "method": "POST",
                    "body_obj": { "deeply": { "nested": long.clone() } },
                }),
            )]),
        ];
        let opts = ExtractOpts {
            include_user_messages: true,
            expected_outcome: None,
            drop_tool_names: vec![],
            metric_names: vec![],
        };
        let fxs = extract_fixtures(&messages, &opts);
        let args = &fxs[0].expected_tool_calls[0].args_schema;
        assert_eq!(args["url"], json!("<elided>"));
        assert_eq!(args["method"], json!("POST"));
        assert_eq!(args["body_obj"]["deeply"]["nested"], json!("<elided>"));
    }

    #[test]
    fn drop_tool_names_filters_out() {
        let messages = vec![
            msg(Role::User, "req"),
            asst_with_calls(vec![
                call("memory_search", json!({})),
                call("http_request", json!({})),
            ]),
        ];
        let opts = ExtractOpts {
            include_user_messages: true,
            expected_outcome: None,
            drop_tool_names: vec!["memory_search".into()],
            metric_names: vec![],
        };
        let fxs = extract_fixtures(&messages, &opts);
        assert_eq!(fxs[0].expected_tool_calls.len(), 1);
        assert_eq!(fxs[0].expected_tool_calls[0].tool, "http_request");
    }

    #[test]
    fn orphan_user_at_end_produces_no_fixture() {
        let messages = vec![
            msg(Role::User, "req1"),
            msg(Role::Assistant, "resp1"),
            msg(Role::User, "orphan"),
        ];
        let opts = ExtractOpts {
            include_user_messages: true,
            expected_outcome: None,
            drop_tool_names: vec![],
            metric_names: vec![],
        };
        let fxs = extract_fixtures(&messages, &opts);
        assert_eq!(fxs.len(), 1);
    }
}
