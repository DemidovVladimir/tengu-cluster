use tengu_core::types::message::Role;
use tengu_core::types::{Message, StreamEvent, ToolDef};

/// Provider-neutral function-tool payload used by backend adapters.
#[derive(Debug, Clone)]
pub(crate) struct FunctionToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Normalize runtime ToolDef structures into backend-friendly function specs.
pub(crate) fn normalize_function_tools(tools: &[ToolDef]) -> Vec<FunctionToolSpec> {
    tools
        .iter()
        .map(|t| FunctionToolSpec {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.parameters.clone(),
        })
        .collect()
}

/// Map normalized tool specs into provider-specific request payload types.
pub(crate) fn map_function_tools<T>(
    tools: &[ToolDef],
    f: impl FnMut(FunctionToolSpec) -> T,
) -> Vec<T> {
    normalize_function_tools(tools).into_iter().map(f).collect()
}

/// Convert runtime messages to an OpenAI-compatible JSON message array.
pub(crate) fn convert_messages_openai_compatible(
    messages: &[Message],
    system_prompt: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut result: Vec<serde_json::Value> = Vec::new();

    if let Some(prompt) = system_prompt.filter(|s| !s.trim().is_empty()) {
        result.push(serde_json::json!({
            "role": "system",
            "content": prompt
        }));
    }

    for m in messages {
        match m.role {
            Role::Tool => {
                let mut msg = serde_json::json!({
                    "role": "tool",
                    "content": m.content
                });
                if let Some(ref id) = m.tool_call_id {
                    msg["tool_call_id"] = serde_json::json!(id);
                }
                result.push(msg);
            }
            Role::Assistant if m.tool_calls.is_some() => {
                let tool_calls: Vec<serde_json::Value> = m
                    .tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|tc| {
                                serde_json::json!({
                                    "id": tc.id,
                                    "type": "function",
                                    "function": {
                                        "name": tc.name,
                                        "arguments": tc.arguments.to_string()
                                    }
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let mut msg = serde_json::json!({
                    "role": "assistant",
                    "tool_calls": tool_calls
                });
                if !m.content.is_empty() {
                    msg["content"] = serde_json::json!(m.content);
                }
                result.push(msg);
            }
            _ => {
                let role = match m.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => unreachable!(),
                };
                result.push(serde_json::json!({
                    "role": role,
                    "content": m.content
                }));
            }
        }
    }

    result
}

/// Append runtime tool-call lifecycle events for one provider response.
pub(crate) fn append_tool_call_events(
    events: &mut Vec<StreamEvent>,
    tool_calls: impl IntoIterator<Item = (String, String, String)>,
) {
    for (id, name, arguments_delta) in tool_calls {
        events.push(StreamEvent::ToolCallStart {
            id: id.clone(),
            name,
        });
        events.push(StreamEvent::ToolCallDelta {
            id: id.clone(),
            arguments_delta,
        });
        events.push(StreamEvent::ToolCallEnd { id });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_function_tools_preserves_fields() {
        let tools = vec![ToolDef {
            name: "read_file".to_string(),
            description: "Read file contents".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            policy: None,
        }];

        let normalized = normalize_function_tools(&tools);
        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0].name, "read_file");
        assert_eq!(normalized[0].description, "Read file contents");
        assert_eq!(normalized[0].parameters["type"], "object");
    }

    #[test]
    fn convert_messages_openai_compatible_handles_tool_messages() {
        let messages = vec![
            Message {
                role: Role::User,
                content: "u".to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                role: Role::Tool,
                content: "tool output".to_string(),
                tool_call_id: Some("call_1".to_string()),
                tool_calls: None,
            },
        ];
        let converted = convert_messages_openai_compatible(&messages, Some("system"));
        assert_eq!(converted[0]["role"], "system");
        assert_eq!(converted[1]["role"], "user");
        assert_eq!(converted[2]["role"], "tool");
        assert_eq!(converted[2]["tool_call_id"], "call_1");
    }

    #[test]
    fn append_tool_call_events_emits_triplets() {
        let mut events = Vec::new();
        append_tool_call_events(
            &mut events,
            vec![(
                "call_1".to_string(),
                "read_file".to_string(),
                r#"{"path":"README.md"}"#.to_string(),
            )],
        );
        assert_eq!(events.len(), 3);
        assert!(
            matches!(events[0], StreamEvent::ToolCallStart { ref name, .. } if name == "read_file")
        );
        assert!(matches!(events[1], StreamEvent::ToolCallDelta { .. }));
        assert!(matches!(events[2], StreamEvent::ToolCallEnd { .. }));
    }
}
