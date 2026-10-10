//! JSON-RPC 2.0 binding of A2A (spec § 9): method names of both dialects,
//! the error codes (§ 5.4), envelopes, and `A2A-Version` negotiation (§ 3.6).
//!
//! | Operation | v1.0 method | 0.3 method |
//! |---|---|---|
//! | send | `SendMessage` | `message/send` |
//! | send, streamed (SSE) | `SendStreamingMessage` | `message/stream` |
//! | get a task | `GetTask` | `tasks/get` |
//! | list tasks | `ListTasks` | — |
//! | cancel | `CancelTask` | `tasks/cancel` |
//! | re-attach to a task's stream | `SubscribeToTask` | `tasks/resubscribe` |
//! | push configs | `Create`/`Get`/`List`/`DeleteTaskPushNotificationConfig(s)` | `tasks/pushNotificationConfig/set`/`get`/`list`/`delete` |
//! | extended card | `GetExtendedAgentCard` | `agent/getAuthenticatedExtendedCard` |
//!
//! | `A2A-Version` header | Dialect |
//! |---|---|
//! | `1.x` | v1.0 — a 0.3 method is `MethodNotFound` |
//! | `0.3` / `0.3.x` | 0.3 — a v1.0 method is `MethodNotFound` |
//! | absent | the method's own (spec: absent means 0.3; tengu also takes a v1.0 method name then) |
//! | anything else | `VersionNotSupportedError` (−32009) naming `1.0` and `0.3` |

use serde_json::{json, Map, Value};

use super::model::{major_minor, Dialect, LEGACY_VERSION, PROTOCOL_VERSION};

/// One A2A operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    SendMessage,
    SendStreamingMessage,
    GetTask,
    ListTasks,
    CancelTask,
    SubscribeToTask,
    CreatePushConfig,
    GetPushConfig,
    ListPushConfigs,
    DeletePushConfig,
    GetExtendedAgentCard,
}

const METHODS: [(Method, &str, Option<&str>); 11] = [
    (Method::SendMessage, "SendMessage", Some("message/send")),
    (
        Method::SendStreamingMessage,
        "SendStreamingMessage",
        Some("message/stream"),
    ),
    (Method::GetTask, "GetTask", Some("tasks/get")),
    (Method::ListTasks, "ListTasks", None),
    (Method::CancelTask, "CancelTask", Some("tasks/cancel")),
    (
        Method::SubscribeToTask,
        "SubscribeToTask",
        Some("tasks/resubscribe"),
    ),
    (
        Method::CreatePushConfig,
        "CreateTaskPushNotificationConfig",
        Some("tasks/pushNotificationConfig/set"),
    ),
    (
        Method::GetPushConfig,
        "GetTaskPushNotificationConfig",
        Some("tasks/pushNotificationConfig/get"),
    ),
    (
        Method::ListPushConfigs,
        "ListTaskPushNotificationConfigs",
        Some("tasks/pushNotificationConfig/list"),
    ),
    (
        Method::DeletePushConfig,
        "DeleteTaskPushNotificationConfig",
        Some("tasks/pushNotificationConfig/delete"),
    ),
    (
        Method::GetExtendedAgentCard,
        "GetExtendedAgentCard",
        Some("agent/getAuthenticatedExtendedCard"),
    ),
];

impl Method {
    /// The method's name in `dialect` (`None`: 0.3 has no `ListTasks`).
    pub fn name(self, dialect: Dialect) -> Option<&'static str> {
        let (_, v1, v03) = METHODS.iter().find(|(m, _, _)| *m == self)?;
        match dialect {
            Dialect::V1 => Some(v1),
            Dialect::V03 => *v03,
        }
    }

    /// The method and the dialect its name belongs to.
    pub fn parse(name: &str) -> Option<(Method, Dialect)> {
        METHODS.iter().find_map(|(m, v1, v03)| {
            if *v1 == name {
                Some((*m, Dialect::V1))
            } else if *v03 == Some(name) {
                Some((*m, Dialect::V03))
            } else {
                None
            }
        })
    }
}

/// A JSON-RPC error object (`code`, `message`, `data`).
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
pub const TASK_NOT_FOUND: i64 = -32001;
pub const TASK_NOT_CANCELABLE: i64 = -32002;
pub const PUSH_NOT_SUPPORTED: i64 = -32003;
pub const UNSUPPORTED_OPERATION: i64 = -32004;
pub const CONTENT_TYPE_NOT_SUPPORTED: i64 = -32005;
pub const INVALID_AGENT_RESPONSE: i64 = -32006;
pub const EXTENDED_CARD_NOT_CONFIGURED: i64 = -32007;
pub const EXTENSION_SUPPORT_REQUIRED: i64 = -32008;
pub const VERSION_NOT_SUPPORTED: i64 = -32009;

/// `(code, error name, ErrorInfo reason)` of every code (§ 5.4, § 9.5).
const ERRORS: [(i64, &str, &str); 14] = [
    (PARSE_ERROR, "JSONParseError", "PARSE_ERROR"),
    (INVALID_REQUEST, "InvalidRequestError", "INVALID_REQUEST"),
    (METHOD_NOT_FOUND, "MethodNotFoundError", "METHOD_NOT_FOUND"),
    (INVALID_PARAMS, "InvalidParamsError", "INVALID_PARAMS"),
    (INTERNAL_ERROR, "InternalError", "INTERNAL"),
    (TASK_NOT_FOUND, "TaskNotFoundError", "TASK_NOT_FOUND"),
    (
        TASK_NOT_CANCELABLE,
        "TaskNotCancelableError",
        "TASK_NOT_CANCELABLE",
    ),
    (
        PUSH_NOT_SUPPORTED,
        "PushNotificationNotSupportedError",
        "PUSH_NOTIFICATION_NOT_SUPPORTED",
    ),
    (
        UNSUPPORTED_OPERATION,
        "UnsupportedOperationError",
        "UNSUPPORTED_OPERATION",
    ),
    (
        CONTENT_TYPE_NOT_SUPPORTED,
        "ContentTypeNotSupportedError",
        "CONTENT_TYPE_NOT_SUPPORTED",
    ),
    (
        INVALID_AGENT_RESPONSE,
        "InvalidAgentResponseError",
        "INVALID_AGENT_RESPONSE",
    ),
    (
        EXTENDED_CARD_NOT_CONFIGURED,
        "ExtendedAgentCardNotConfiguredError",
        "EXTENDED_AGENT_CARD_NOT_CONFIGURED",
    ),
    (
        EXTENSION_SUPPORT_REQUIRED,
        "ExtensionSupportRequiredError",
        "EXTENSION_SUPPORT_REQUIRED",
    ),
    (
        VERSION_NOT_SUPPORTED,
        "VersionNotSupportedError",
        "VERSION_NOT_SUPPORTED",
    ),
];

impl RpcError {
    /// An error with a `google.rpc.ErrorInfo` detail (`reason` from the
    /// code, `metadata` = `meta`).
    pub fn new(code: i64, message: impl Into<String>, meta: Value) -> Self {
        let reason = ERRORS
            .iter()
            .find(|(c, _, _)| *c == code)
            .map_or("ERROR", |(_, _, r)| *r);
        let mut info = json!({
            "@type": "type.googleapis.com/google.rpc.ErrorInfo",
            "reason": reason,
            "domain": "a2a-protocol.org",
        });
        if meta.as_object().is_some_and(|m| !m.is_empty()) {
            info["metadata"] = meta;
        }
        Self {
            code,
            message: message.into(),
            data: Some(json!([info])),
        }
    }

    /// The A2A / JSON-RPC name of the code (`TaskNotFoundError`), else
    /// `error <code>`.
    pub fn name(&self) -> String {
        ERRORS
            .iter()
            .find(|(c, _, _)| *c == self.code)
            .map_or_else(|| format!("error {}", self.code), |(_, n, _)| n.to_string())
    }

    pub fn parse(why: impl Into<String>) -> Self {
        Self::new(PARSE_ERROR, why, json!({}))
    }

    pub fn invalid_request(why: impl Into<String>) -> Self {
        Self::new(INVALID_REQUEST, why, json!({}))
    }

    pub fn method_not_found(method: &str, why: &str) -> Self {
        Self::new(
            METHOD_NOT_FOUND,
            format!("Method not found: {method}{why}"),
            json!({"method": method}),
        )
    }

    pub fn invalid_params(field: &str, why: impl Into<String>) -> Self {
        let why = why.into();
        let mut e = Self::new(
            INVALID_PARAMS,
            format!("Invalid parameters: {field}: {why}"),
            json!({}),
        );
        e.data = Some(json!([{
            "@type": "type.googleapis.com/google.rpc.BadRequest",
            "fieldViolations": [{"field": field, "description": why}],
        }]));
        e
    }

    pub fn internal(why: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, why, json!({}))
    }

    pub fn task_not_found(id: &str) -> Self {
        Self::new(TASK_NOT_FOUND, "Task not found", json!({"taskId": id}))
    }

    pub fn not_cancelable(id: &str, state: &str) -> Self {
        Self::new(
            TASK_NOT_CANCELABLE,
            format!("Task is not cancelable: it is {state}"),
            json!({"taskId": id, "state": state}),
        )
    }

    pub fn unsupported(why: impl Into<String>) -> Self {
        Self::new(UNSUPPORTED_OPERATION, why, json!({}))
    }

    pub fn push_not_supported() -> Self {
        Self::new(
            PUSH_NOT_SUPPORTED,
            "Push notifications are not supported by this agent",
            json!({}),
        )
    }

    pub fn content_type(why: impl Into<String>) -> Self {
        Self::new(CONTENT_TYPE_NOT_SUPPORTED, why, json!({}))
    }

    pub fn version_not_supported(asked: &str) -> Self {
        Self::new(
            VERSION_NOT_SUPPORTED,
            format!("A2A protocol version {asked} is not supported (supported: {PROTOCOL_VERSION}, {LEGACY_VERSION})"),
            json!({"requestedVersion": asked, "supportedVersions": format!("{PROTOCOL_VERSION},{LEGACY_VERSION}")}),
        )
    }

    /// The JSON-RPC error object.
    pub fn to_json(&self) -> Value {
        let mut e = json!({"code": self.code, "message": self.message});
        if let Some(d) = &self.data {
            e["data"] = d.clone();
        }
        e
    }

    /// Read a JSON-RPC error object (a client reading a reply).
    pub fn from_json(v: &Value) -> Self {
        Self {
            code: v
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or(INTERNAL_ERROR),
            message: v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            data: v.get("data").cloned(),
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.name(), self.code, self.message)
    }
}

/// A JSON-RPC request object.
pub fn request(id: &Value, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// A success response.
pub fn success(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// An error response.
pub fn failure(id: &Value, error: &RpcError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": error.to_json()})
}

/// A parsed incoming request.
#[derive(Debug, Clone, PartialEq)]
pub struct Incoming {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

/// Parse a request body. `Err` carries the id to answer with (`null` when
/// none could be read) and the error. Notifications (no `id`) are answered
/// as `InvalidRequest`: every A2A method returns something.
pub fn parse_request(body: &[u8]) -> Result<Incoming, (Value, RpcError)> {
    let v: Value = serde_json::from_slice(body).map_err(|e| {
        (
            Value::Null,
            RpcError::parse(format!("Invalid JSON payload: {e}")),
        )
    })?;
    let Some(obj) = v.as_object() else {
        return Err((
            Value::Null,
            RpcError::invalid_request(
                "Request payload validation error: not an object (batches are not supported)",
            ),
        ));
    };
    let id = obj.get("id").cloned().unwrap_or(Value::Null);
    let valid_id = matches!(id, Value::String(_) | Value::Number(_));
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err((
            if valid_id { id } else { Value::Null },
            RpcError::invalid_request(
                "Request payload validation error: `jsonrpc` must be \"2.0\"",
            ),
        ));
    }
    if !valid_id {
        return Err((
            Value::Null,
            RpcError::invalid_request(
                "Request payload validation error: `id` must be a string or a number",
            ),
        ));
    }
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return Err((
            id,
            RpcError::invalid_request(
                "Request payload validation error: `method` must be a string",
            ),
        ));
    };
    let params = match obj.get("params") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(p @ Value::Object(_)) => p.clone(),
        Some(_) => {
            return Err((id, RpcError::invalid_params("params", "must be an object")));
        }
    };
    Ok(Incoming {
        id,
        method: method.to_string(),
        params,
    })
}

/// A client reading a JSON-RPC response: the `result`, or the `error`.
pub fn parse_response(v: &Value) -> Result<Value, RpcError> {
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        return Err(RpcError::from_json(e));
    }
    v.get("result").cloned().ok_or_else(|| {
        RpcError::new(
            INVALID_AGENT_RESPONSE,
            "response has neither `result` nor `error`",
            json!({}),
        )
    })
}

/// What an `A2A-Version` header asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionAsk {
    Absent,
    Speaks(Dialect),
    Unsupported(String),
}

/// Read an `A2A-Version` header value (module table).
pub fn version_ask(header: Option<&str>) -> VersionAsk {
    let Some(raw) = header.map(str::trim).filter(|h| !h.is_empty()) else {
        return VersionAsk::Absent;
    };
    match major_minor(raw) {
        Some((1, _)) => VersionAsk::Speaks(Dialect::V1),
        Some((0, 3)) => VersionAsk::Speaks(Dialect::V03),
        _ => VersionAsk::Unsupported(raw.to_string()),
    }
}

/// The dialect to answer `method` in (module table).
pub fn negotiate(header: Option<&str>, method: &str) -> Result<(Method, Dialect), RpcError> {
    let parsed = Method::parse(method);
    match (version_ask(header), parsed) {
        (VersionAsk::Unsupported(v), _) => Err(RpcError::version_not_supported(&v)),
        (_, None) => Err(RpcError::method_not_found(method, "")),
        (VersionAsk::Absent, Some(found)) => Ok(found),
        (VersionAsk::Speaks(want), Some((m, d))) if want == d => Ok((m, d)),
        (VersionAsk::Speaks(want), Some(_)) => Err(RpcError::method_not_found(
            method,
            &format!(" (not a method of A2A {})", want.version()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_round_trips_its_names() {
        for (m, v1, v03) in METHODS {
            assert_eq!(Method::parse(v1), Some((m, Dialect::V1)));
            assert_eq!(m.name(Dialect::V1), Some(v1));
            if let Some(v03) = v03 {
                assert_eq!(Method::parse(v03), Some((m, Dialect::V03)));
            }
            assert_eq!(m.name(Dialect::V03), v03);
        }
        assert_eq!(Method::parse("message/sned"), None);
    }

    #[test]
    fn version_header_picks_the_dialect() {
        assert_eq!(
            negotiate(Some("1.0"), "SendMessage"),
            Ok((Method::SendMessage, Dialect::V1))
        );
        assert_eq!(
            negotiate(None, "message/send"),
            Ok((Method::SendMessage, Dialect::V03))
        );
        assert_eq!(
            negotiate(None, "GetTask"),
            Ok((Method::GetTask, Dialect::V1))
        );
        assert_eq!(
            negotiate(Some("1.0"), "message/send").unwrap_err().code,
            METHOD_NOT_FOUND
        );
        assert_eq!(
            negotiate(Some("0.3"), "SendMessage").unwrap_err().code,
            METHOD_NOT_FOUND
        );
        let e = negotiate(Some("0.5"), "SendMessage").unwrap_err();
        assert_eq!(e.code, VERSION_NOT_SUPPORTED);
        assert!(e.message.contains("0.5"), "{e}");
        assert_eq!(negotiate(None, "Nope").unwrap_err().code, METHOD_NOT_FOUND);
    }

    #[test]
    fn requests_parse_strictly() {
        let ok =
            parse_request(br#"{"jsonrpc":"2.0","id":7,"method":"GetTask","params":{"id":"t"}}"#)
                .unwrap();
        assert_eq!(ok.id, json!(7));
        assert_eq!(ok.params["id"], "t");
        let (id, e) = parse_request(b"{nope").unwrap_err();
        assert_eq!((id, e.code), (Value::Null, PARSE_ERROR));
        let (id, e) = parse_request(br#"{"jsonrpc":"1.0","id":"a","method":"x"}"#).unwrap_err();
        assert_eq!((id, e.code), (json!("a"), INVALID_REQUEST));
        let (_, e) = parse_request(br#"{"jsonrpc":"2.0","method":"x"}"#).unwrap_err();
        assert_eq!(e.code, INVALID_REQUEST);
        let (_, e) =
            parse_request(br#"{"jsonrpc":"2.0","id":1,"method":"x","params":[1]}"#).unwrap_err();
        assert_eq!(e.code, INVALID_PARAMS);
        let none = parse_request(br#"{"jsonrpc":"2.0","id":1,"method":"x"}"#).unwrap();
        assert_eq!(none.params, json!({}));
    }

    #[test]
    fn errors_carry_error_info() {
        let e = RpcError::task_not_found("t-1");
        let v = failure(&json!(2), &e);
        assert_eq!(v["error"]["code"], -32001);
        assert_eq!(v["error"]["data"][0]["reason"], "TASK_NOT_FOUND");
        assert_eq!(v["error"]["data"][0]["metadata"]["taskId"], "t-1");
        assert_eq!(e.name(), "TaskNotFoundError");
        let back = parse_response(&v).unwrap_err();
        assert_eq!(back.code, -32001);
        assert_eq!(
            parse_response(&success(&json!(1), json!({"ok": 1}))).unwrap(),
            json!({"ok": 1})
        );
    }
}
