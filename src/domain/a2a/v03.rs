//! A2A 0.3 dialect ↔ the v1.0 model (`model.rs`), on JSON values. tengu
//! serves 0.3 clients and calls 0.3 agents through these; every other file
//! sees v1.0 types only.
//!
//! | Object | 0.3 form | v1.0 form |
//! |---|---|---|
//! | part | `{"kind": "text", "text"}` · `{"kind": "data", "data": {…}}` (an object) · `{"kind": "file", "file": {"uri" \| "bytes", "mimeType", "name"}}` | `{"text"}` · `{"data"}` (any value) · `{"url" \| "raw", "mediaType", "filename"}` |
//! | role | `"user"` / `"agent"` | `"ROLE_USER"` / `"ROLE_AGENT"` |
//! | message / task | `"kind": "message"` / `"kind": "task"`; states `"completed"`, `"input-required"` … | no `kind`; `"TASK_STATE_COMPLETED"` … |
//! | send result | the task or message itself | `{"task": …}` / `{"message": …}` |
//! | stream event | `kind` `status-update` (with `final`) / `artifact-update` | `{"statusUpdate"}` / `{"artifactUpdate"}` |
//! | send configuration | `blocking` (missing = blocking, as the 0.3 SDKs did) | `returnImmediately` |
//! | card | `protocolVersion`, `url`, `preferredTransport`; OpenAPI-style `securitySchemes` (`{"type": "http", "scheme": "bearer"}`) + `security` | `supportedInterfaces`; `{"httpAuthSecurityScheme": {"scheme": "Bearer"}}` + `securityRequirements` |
//!
//! A non-object v1.0 `data` part is wrapped as `{"value": <data>}` for 0.3.

use serde_json::{json, Map, Value};

use super::model::{
    AgentCard, Artifact, GetTaskRequest, Message, Part, Role, SendMessageConfiguration,
    SendMessageRequest, SendResult, StreamResponse, Task, TaskArtifactUpdateEvent, TaskIdRequest,
    TaskState, TaskStatus, TaskStatusUpdateEvent, JSONRPC,
};

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn obj(v: &Value, key: &str) -> Option<Map<String, Value>> {
    v.get(key).and_then(Value::as_object).cloned()
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn insert_opt(m: &mut Map<String, Value>, key: &str, v: Option<Value>) {
    if let Some(v) = v {
        m.insert(key.into(), v);
    }
}

// ── 0.3 → v1.0 ──────────────────────────────────────────────────────────

/// A 0.3 part (by `kind`, else by which field it has).
pub fn part_from_v03(v: &Value) -> Part {
    let kind = v.get("kind").and_then(Value::as_str);
    let metadata = obj(v, "metadata");
    match kind {
        Some("data") => Part {
            data: v.get("data").cloned(),
            media_type: Some("application/json".into()),
            metadata,
            ..Default::default()
        },
        Some("file") => {
            let f = v.get("file").cloned().unwrap_or(Value::Null);
            Part {
                url: s(&f, "uri"),
                raw: s(&f, "bytes"),
                media_type: s(&f, "mimeType"),
                filename: s(&f, "name"),
                metadata,
                ..Default::default()
            }
        }
        Some("text") => Part {
            text: Some(s(v, "text").unwrap_or_default()),
            metadata,
            ..Default::default()
        },
        // No `kind`: maybe already v1.0-shaped.
        _ if v.get("file").is_some() => part_from_v03(&{
            let mut c = v.clone();
            c["kind"] = json!("file");
            c
        }),
        _ => serde_json::from_value(v.clone()).unwrap_or_default(),
    }
}

/// A 0.3 message.
pub fn message_from_v03(v: &Value) -> Message {
    Message {
        message_id: s(v, "messageId").unwrap_or_default(),
        context_id: s(v, "contextId"),
        task_id: s(v, "taskId"),
        role: Role::parse(v.get("role").and_then(Value::as_str).unwrap_or("")),
        parts: v
            .get("parts")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(part_from_v03).collect())
            .unwrap_or_default(),
        metadata: obj(v, "metadata"),
        extensions: strings(v, "extensions"),
        reference_task_ids: strings(v, "referenceTaskIds"),
    }
}

fn status_from_v03(v: &Value) -> TaskStatus {
    TaskStatus {
        state: v
            .get("state")
            .and_then(Value::as_str)
            .and_then(TaskState::parse)
            .unwrap_or_default(),
        message: v
            .get("message")
            .filter(|m| m.is_object())
            .map(message_from_v03),
        timestamp: s(v, "timestamp"),
    }
}

/// A 0.3 artifact.
pub fn artifact_from_v03(v: &Value) -> Artifact {
    Artifact {
        artifact_id: s(v, "artifactId").unwrap_or_default(),
        name: s(v, "name"),
        description: s(v, "description"),
        parts: v
            .get("parts")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(part_from_v03).collect())
            .unwrap_or_default(),
        metadata: obj(v, "metadata"),
        extensions: strings(v, "extensions"),
    }
}

/// A 0.3 task.
pub fn task_from_v03(v: &Value) -> Task {
    Task {
        id: s(v, "id").unwrap_or_default(),
        context_id: s(v, "contextId").unwrap_or_default(),
        status: v.get("status").map(status_from_v03).unwrap_or_default(),
        artifacts: v
            .get("artifacts")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(artifact_from_v03).collect()),
        history: v
            .get("history")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(message_from_v03).collect()),
        metadata: obj(v, "metadata"),
    }
}

/// 0.3 `MessageSendParams` → `SendMessageRequest`.
pub fn send_request_from_v03(params: &Value) -> SendMessageRequest {
    let c = params.get("configuration");
    SendMessageRequest {
        tenant: None,
        message: params
            .get("message")
            .map(message_from_v03)
            .unwrap_or_default(),
        configuration: c
            .filter(|c| c.is_object())
            .map(|c| SendMessageConfiguration {
                accepted_output_modes: strings(c, "acceptedOutputModes"),
                task_push_notification_config: c.get("pushNotificationConfig").cloned(),
                history_length: c.get("historyLength").and_then(Value::as_i64),
                return_immediately: c.get("blocking").and_then(Value::as_bool) == Some(false),
            }),
        metadata: obj(params, "metadata"),
    }
}

/// 0.3 `TaskQueryParams` → `GetTaskRequest`.
pub fn get_request_from_v03(params: &Value) -> GetTaskRequest {
    GetTaskRequest {
        tenant: None,
        id: s(params, "id").unwrap_or_default(),
        history_length: params.get("historyLength").and_then(Value::as_i64),
    }
}

/// 0.3 `TaskIdParams` → `TaskIdRequest`.
pub fn task_id_request_from_v03(params: &Value) -> TaskIdRequest {
    TaskIdRequest {
        tenant: None,
        id: s(params, "id").unwrap_or_default(),
        metadata: obj(params, "metadata"),
    }
}

/// A 0.3 `message/send` result (the task or message itself; a v1.0-shaped
/// `{"task"}` / `{"message"}` is read too).
pub fn send_result_from_v03(v: &Value) -> Result<SendResult, String> {
    match v.get("kind").and_then(Value::as_str) {
        Some("task") => Ok(SendResult::Task(task_from_v03(v))),
        Some("message") => Ok(SendResult::Message(message_from_v03(v))),
        Some(other) => Err(format!("unexpected result kind `{other}`")),
        None if v.get("task").is_some() => Ok(SendResult::Task(task_from_v03(&v["task"]))),
        None if v.get("message").is_some() => {
            Ok(SendResult::Message(message_from_v03(&v["message"])))
        }
        None if v.get("status").is_some() => Ok(SendResult::Task(task_from_v03(v))),
        None if v.get("parts").is_some() => Ok(SendResult::Message(message_from_v03(v))),
        None => Err("result is neither a task nor a message".into()),
    }
}

/// A 0.3 stream event (tengu's client polls; a streaming reader's half).
#[cfg_attr(not(test), allow(dead_code))]
pub fn stream_from_v03(v: &Value) -> Result<StreamResponse, String> {
    match v.get("kind").and_then(Value::as_str) {
        Some("status-update") => Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: s(v, "taskId").unwrap_or_default(),
            context_id: s(v, "contextId").unwrap_or_default(),
            status: v.get("status").map(status_from_v03).unwrap_or_default(),
            metadata: obj(v, "metadata"),
        })),
        Some("artifact-update") => Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            task_id: s(v, "taskId").unwrap_or_default(),
            context_id: s(v, "contextId").unwrap_or_default(),
            artifact: v.get("artifact").map(artifact_from_v03).unwrap_or_default(),
            append: v.get("append").and_then(Value::as_bool).unwrap_or(false),
            last_chunk: v.get("lastChunk").and_then(Value::as_bool).unwrap_or(false),
            metadata: obj(v, "metadata"),
        })),
        _ => send_result_from_v03(v).map(|r| match r {
            SendResult::Task(t) => StreamResponse::Task(t),
            SendResult::Message(m) => StreamResponse::Message(m),
        }),
    }
}

// ── v1.0 → 0.3 ──────────────────────────────────────────────────────────

/// A part in 0.3 form.
pub fn part_to_v03(p: &Part) -> Value {
    let mut m = Map::new();
    if let Some(d) = &p.data {
        m.insert("kind".into(), json!("data"));
        let data = if d.is_object() {
            d.clone()
        } else {
            json!({"value": d})
        };
        m.insert("data".into(), data);
    } else if p.url.is_some() || p.raw.is_some() {
        m.insert("kind".into(), json!("file"));
        let mut f = Map::new();
        insert_opt(&mut f, "uri", p.url.clone().map(Value::from));
        insert_opt(&mut f, "bytes", p.raw.clone().map(Value::from));
        insert_opt(&mut f, "mimeType", p.media_type.clone().map(Value::from));
        insert_opt(&mut f, "name", p.filename.clone().map(Value::from));
        m.insert("file".into(), Value::Object(f));
    } else {
        m.insert("kind".into(), json!("text"));
        m.insert("text".into(), json!(p.text.clone().unwrap_or_default()));
    }
    insert_opt(&mut m, "metadata", p.metadata.clone().map(Value::Object));
    Value::Object(m)
}

/// A message in 0.3 form.
pub fn message_to_v03(msg: &Message) -> Value {
    let mut m = Map::new();
    m.insert("kind".into(), json!("message"));
    m.insert("messageId".into(), json!(msg.message_id));
    m.insert("role".into(), json!(msg.role.v03_name()));
    m.insert(
        "parts".into(),
        Value::Array(msg.parts.iter().map(part_to_v03).collect()),
    );
    insert_opt(&mut m, "contextId", msg.context_id.clone().map(Value::from));
    insert_opt(&mut m, "taskId", msg.task_id.clone().map(Value::from));
    insert_opt(&mut m, "metadata", msg.metadata.clone().map(Value::Object));
    if !msg.extensions.is_empty() {
        m.insert("extensions".into(), json!(msg.extensions));
    }
    if !msg.reference_task_ids.is_empty() {
        m.insert("referenceTaskIds".into(), json!(msg.reference_task_ids));
    }
    Value::Object(m)
}

fn status_to_v03(st: &TaskStatus) -> Value {
    let mut m = Map::new();
    m.insert("state".into(), json!(st.state.v03_name()));
    insert_opt(&mut m, "message", st.message.as_ref().map(message_to_v03));
    insert_opt(&mut m, "timestamp", st.timestamp.clone().map(Value::from));
    Value::Object(m)
}

/// An artifact in 0.3 form.
pub fn artifact_to_v03(a: &Artifact) -> Value {
    let mut m = Map::new();
    m.insert("artifactId".into(), json!(a.artifact_id));
    m.insert(
        "parts".into(),
        Value::Array(a.parts.iter().map(part_to_v03).collect()),
    );
    insert_opt(&mut m, "name", a.name.clone().map(Value::from));
    insert_opt(
        &mut m,
        "description",
        a.description.clone().map(Value::from),
    );
    insert_opt(&mut m, "metadata", a.metadata.clone().map(Value::Object));
    if !a.extensions.is_empty() {
        m.insert("extensions".into(), json!(a.extensions));
    }
    Value::Object(m)
}

/// A task in 0.3 form.
pub fn task_to_v03(t: &Task) -> Value {
    let mut m = Map::new();
    m.insert("kind".into(), json!("task"));
    m.insert("id".into(), json!(t.id));
    m.insert("contextId".into(), json!(t.context_id));
    m.insert("status".into(), status_to_v03(&t.status));
    insert_opt(
        &mut m,
        "artifacts",
        t.artifacts
            .as_ref()
            .map(|a| Value::Array(a.iter().map(artifact_to_v03).collect())),
    );
    insert_opt(
        &mut m,
        "history",
        t.history
            .as_ref()
            .map(|h| Value::Array(h.iter().map(message_to_v03).collect())),
    );
    insert_opt(&mut m, "metadata", t.metadata.clone().map(Value::Object));
    Value::Object(m)
}

/// `SendMessageRequest` → 0.3 `MessageSendParams` (`blocking` always
/// written).
pub fn send_request_to_v03(r: &SendMessageRequest) -> Value {
    let mut m = Map::new();
    m.insert("message".into(), message_to_v03(&r.message));
    let c = r.configuration.clone().unwrap_or_default();
    let mut cfg = Map::new();
    cfg.insert("blocking".into(), json!(!c.return_immediately));
    if !c.accepted_output_modes.is_empty() {
        cfg.insert("acceptedOutputModes".into(), json!(c.accepted_output_modes));
    }
    insert_opt(&mut cfg, "historyLength", c.history_length.map(Value::from));
    insert_opt(
        &mut cfg,
        "pushNotificationConfig",
        c.task_push_notification_config,
    );
    m.insert("configuration".into(), Value::Object(cfg));
    insert_opt(&mut m, "metadata", r.metadata.clone().map(Value::Object));
    Value::Object(m)
}

/// A stream event in 0.3 form (`final` = the status is settled).
pub fn stream_to_v03(e: &StreamResponse) -> Value {
    match e {
        StreamResponse::Task(t) => task_to_v03(t),
        StreamResponse::Message(m) => message_to_v03(m),
        StreamResponse::StatusUpdate(u) => {
            let mut m = Map::new();
            m.insert("kind".into(), json!("status-update"));
            m.insert("taskId".into(), json!(u.task_id));
            m.insert("contextId".into(), json!(u.context_id));
            m.insert("status".into(), status_to_v03(&u.status));
            m.insert("final".into(), json!(u.status.state.is_settled()));
            insert_opt(&mut m, "metadata", u.metadata.clone().map(Value::Object));
            Value::Object(m)
        }
        StreamResponse::ArtifactUpdate(u) => {
            let mut m = Map::new();
            m.insert("kind".into(), json!("artifact-update"));
            m.insert("taskId".into(), json!(u.task_id));
            m.insert("contextId".into(), json!(u.context_id));
            m.insert("artifact".into(), artifact_to_v03(&u.artifact));
            m.insert("append".into(), json!(u.append));
            m.insert("lastChunk".into(), json!(u.last_chunk));
            insert_opt(&mut m, "metadata", u.metadata.clone().map(Value::Object));
            Value::Object(m)
        }
    }
}

/// One v1.0 security scheme in OpenAPI (0.3) form; `None` for a scheme
/// with no 0.3 equivalent.
fn scheme_to_v03(v: &Value) -> Option<Value> {
    if let Some(h) = v.get("httpAuthSecurityScheme") {
        let mut m = json!({
            "type": "http",
            "scheme": s(h, "scheme").unwrap_or_default().to_ascii_lowercase(),
        });
        if let Some(f) = s(h, "bearerFormat") {
            m["bearerFormat"] = json!(f);
        }
        return Some(m);
    }
    if let Some(k) = v.get("apiKeySecurityScheme") {
        return Some(json!({
            "type": "apiKey",
            "in": s(k, "location").unwrap_or_default(),
            "name": s(k, "name").unwrap_or_default(),
        }));
    }
    None
}

/// The card a 0.3 client reads: `protocolVersion` 0.3.0, `url` +
/// `preferredTransport` = `url_03` (the JSON-RPC endpoint), the
/// `supportedInterfaces` dropped, the security schemes in OpenAPI form.
pub fn card_to_v03(card: &AgentCard, url_03: &str) -> Value {
    let mut v = serde_json::to_value(card).unwrap_or_else(|_| json!({}));
    let Some(m) = v.as_object_mut() else {
        return v;
    };
    for key in [
        "supportedInterfaces",
        "securitySchemes",
        "securityRequirements",
        "additionalInterfaces",
    ] {
        m.remove(key);
    }
    m.insert("protocolVersion".into(), json!("0.3.0"));
    m.insert("url".into(), json!(url_03));
    m.insert("preferredTransport".into(), json!(JSONRPC));
    if let Some(schemes) = &card.security_schemes {
        let converted: Map<String, Value> = schemes
            .iter()
            .filter_map(|(k, v)| scheme_to_v03(v).map(|s| (k.clone(), s)))
            .collect();
        if !converted.is_empty() {
            m.insert("securitySchemes".into(), Value::Object(converted));
        }
    }
    let security: Vec<Value> = card
        .security_requirements
        .iter()
        .flatten()
        .filter_map(|r| r.get("schemes").and_then(Value::as_object))
        .map(|schemes| {
            Value::Object(
                schemes
                    .iter()
                    .map(|(k, scopes)| {
                        let list = scopes.get("list").cloned().unwrap_or_else(|| json!([]));
                        (k.clone(), list)
                    })
                    .collect(),
            )
        })
        .collect();
    if !security.is_empty() {
        m.insert("security".into(), Value::Array(security));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_v03_send_reads_as_v1() {
        let params = json!({
            "message": {"kind": "message", "messageId": "m1", "role": "user", "contextId": "c1",
                        "parts": [{"kind": "text", "text": "hi"},
                                  {"kind": "data", "data": {"x": 1}},
                                  {"kind": "file", "file": {"uri": "https://f/a.pdf", "mimeType": "application/pdf", "name": "a.pdf"}}]},
            "configuration": {"blocking": false, "historyLength": 2}
        });
        let r = send_request_from_v03(&params);
        assert_eq!(r.message.role, Role::User);
        assert_eq!(r.message.context_id.as_deref(), Some("c1"));
        assert_eq!(r.message.parts[0].text.as_deref(), Some("hi"));
        assert_eq!(r.message.parts[1].data, Some(json!({"x": 1})));
        assert_eq!(r.message.parts[2].url.as_deref(), Some("https://f/a.pdf"));
        assert_eq!(r.message.parts[2].filename.as_deref(), Some("a.pdf"));
        let c = r.configuration.unwrap();
        assert!(c.return_immediately);
        assert_eq!(c.history_length, Some(2));
        // Missing `blocking` = blocking (the 0.3 SDKs).
        let blocking = send_request_from_v03(&json!({"message": {}, "configuration": {}}));
        assert!(!blocking.configuration.unwrap().return_immediately);
    }

    #[test]
    fn a_task_round_trips_through_v03() {
        let t = Task {
            id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::InputRequired,
                message: Some(Message::agent_text("m2".into(), "more?")),
                timestamp: Some("2026-10-10T10:00:00.000Z".into()),
            },
            artifacts: Some(vec![Artifact {
                artifact_id: "a1".into(),
                parts: vec![Part::text("out"), Part::data(json!([1, 2]))],
                ..Default::default()
            }]),
            history: None,
            metadata: None,
        };
        let v = task_to_v03(&t);
        assert_eq!(v["kind"], "task");
        assert_eq!(v["status"]["state"], "input-required");
        assert_eq!(v["status"]["message"]["role"], "agent");
        assert_eq!(
            v["artifacts"][0]["parts"][1]["data"],
            json!({"value": [1, 2]})
        );
        let back = task_from_v03(&v);
        assert_eq!(back.status.state, TaskState::InputRequired);
        assert_eq!(
            back.artifacts.unwrap()[0].parts[0].text.as_deref(),
            Some("out")
        );
        match send_result_from_v03(&v).unwrap() {
            SendResult::Task(t) => assert_eq!(t.id, "t1"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stream_events_carry_final() {
        let done = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t".into(),
            context_id: "c".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                ..Default::default()
            },
            metadata: None,
        });
        let v = stream_to_v03(&done);
        assert_eq!(
            (v["kind"].as_str(), v["final"].as_bool()),
            (Some("status-update"), Some(true))
        );
        assert_eq!(stream_from_v03(&v).unwrap(), done);
    }

    #[test]
    fn the_v03_card_names_its_url_and_openapi_schemes() {
        let card: AgentCard = serde_json::from_value(json!({
            "name": "n", "description": "d", "version": "1",
            "supportedInterfaces": [{"url": "https://x/a2a", "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}],
            "securitySchemes": {"bearer": {"httpAuthSecurityScheme": {"scheme": "Bearer"}}},
            "securityRequirements": [{"schemes": {"bearer": {"list": []}}}],
            "capabilities": {"streaming": true}, "defaultInputModes": ["text/plain"],
            "defaultOutputModes": ["text/plain"], "skills": []
        }))
        .unwrap();
        let v = card_to_v03(&card, "https://x/a2a");
        assert_eq!(v["protocolVersion"], "0.3.0");
        assert_eq!(v["url"], "https://x/a2a");
        assert_eq!(v["preferredTransport"], "JSONRPC");
        assert!(v.get("supportedInterfaces").is_none());
        assert_eq!(
            v["securitySchemes"]["bearer"],
            json!({"type": "http", "scheme": "bearer"})
        );
        assert_eq!(v["security"], json!([{"bearer": []}]));
    }
}
