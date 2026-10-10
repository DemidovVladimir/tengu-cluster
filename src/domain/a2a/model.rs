//! A2A v1.0 data model — the JSON form of `a2a.proto` (`lf.a2a.v1`, spec
//! 1.0.1): camelCase fields, enums as their proto names
//! (`TASK_STATE_COMPLETED`, `ROLE_USER`), each `oneof` as one key
//! (`{"task": …}`; a [`Part`] holds exactly one of `text` / `raw` / `url` /
//! `data`).
//!
//! | Direction | Rule |
//! |---|---|
//! | read | lenient (spec § 5.7): unknown fields ignored, missing ones defaulted, 0.3 spellings of states and roles (`completed`, `user`) accepted — the server validates what it needs |
//! | write | the v1.0 form only; empty optional fields omitted (ProtoJSON) |
//!
//! [`AgentCard`] also carries the 0.3 top-level fields (`url`,
//! `protocolVersion`, `preferredTransport`, `additionalInterfaces`): read from
//! a 0.3 card, written into tengu's own card for 0.3 clients (`card.rs`).

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// The protocol version tengu speaks natively (`A2A-Version`, interfaces).
pub const PROTOCOL_VERSION: &str = "1.0";
/// The older version tengu also serves and calls (`v03.rs`).
pub const LEGACY_VERSION: &str = "0.3";
/// Binding names of `AgentInterface.protocolBinding`.
pub const JSONRPC: &str = "JSONRPC";
pub const HTTP_JSON: &str = "HTTP+JSON";

/// Sender of a [`Message`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Role {
    #[default]
    Unspecified,
    User,
    Agent,
}

impl Role {
    pub fn proto_name(self) -> &'static str {
        match self {
            Role::Unspecified => "ROLE_UNSPECIFIED",
            Role::User => "ROLE_USER",
            Role::Agent => "ROLE_AGENT",
        }
    }

    /// The 0.3 spelling (`user` / `agent`).
    pub fn v03_name(self) -> &'static str {
        match self {
            Role::Agent => "agent",
            _ => "user",
        }
    }

    /// Either spelling; anything else is `Unspecified`.
    pub fn parse(s: &str) -> Self {
        match s {
            "ROLE_USER" | "user" => Role::User,
            "ROLE_AGENT" | "agent" => Role::Agent,
            _ => Role::Unspecified,
        }
    }
}

impl Serialize for Role {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.proto_name())
    }
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Role::parse(&String::deserialize(d)?))
    }
}

/// Lifecycle state of a [`Task`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum TaskState {
    #[default]
    Unspecified,
    Submitted,
    Working,
    Completed,
    Failed,
    Canceled,
    InputRequired,
    Rejected,
    AuthRequired,
}

impl TaskState {
    /// Every state a client may filter on (`ListTasks.status`).
    pub const ALL: [TaskState; 8] = [
        TaskState::Submitted,
        TaskState::Working,
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Canceled,
        TaskState::InputRequired,
        TaskState::Rejected,
        TaskState::AuthRequired,
    ];

    pub fn proto_name(self) -> &'static str {
        match self {
            TaskState::Unspecified => "TASK_STATE_UNSPECIFIED",
            TaskState::Submitted => "TASK_STATE_SUBMITTED",
            TaskState::Working => "TASK_STATE_WORKING",
            TaskState::Completed => "TASK_STATE_COMPLETED",
            TaskState::Failed => "TASK_STATE_FAILED",
            TaskState::Canceled => "TASK_STATE_CANCELED",
            TaskState::InputRequired => "TASK_STATE_INPUT_REQUIRED",
            TaskState::Rejected => "TASK_STATE_REJECTED",
            TaskState::AuthRequired => "TASK_STATE_AUTH_REQUIRED",
        }
    }

    /// The 0.3 spelling (`input-required`, …; `unknown` for unspecified).
    pub fn v03_name(self) -> &'static str {
        match self {
            TaskState::Unspecified => "unknown",
            TaskState::Submitted => "submitted",
            TaskState::Working => "working",
            TaskState::Completed => "completed",
            TaskState::Failed => "failed",
            TaskState::Canceled => "canceled",
            TaskState::InputRequired => "input-required",
            TaskState::Rejected => "rejected",
            TaskState::AuthRequired => "auth-required",
        }
    }

    /// Either spelling, exactly (`None` = no state of either version).
    pub fn parse(s: &str) -> Option<Self> {
        std::iter::once(TaskState::Unspecified)
            .chain(Self::ALL)
            .find(|t| t.proto_name() == s || t.v03_name() == s)
            // 0.3 also spelled it `cancelled` in some SDKs.
            .or_else(|| (s == "cancelled").then_some(TaskState::Canceled))
    }

    /// `COMPLETED`, `FAILED`, `CANCELED`, `REJECTED`: the task takes no
    /// more messages.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected
        )
    }

    /// `INPUT_REQUIRED`, `AUTH_REQUIRED`: the agent waits for the client.
    pub fn is_interrupted(self) -> bool {
        matches!(self, TaskState::InputRequired | TaskState::AuthRequired)
    }

    /// Where a blocking `SendMessage` returns: terminal or interrupted.
    pub fn is_settled(self) -> bool {
        self.is_terminal() || self.is_interrupted()
    }
}

impl Serialize for TaskState {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.proto_name())
    }
}

impl<'de> Deserialize<'de> for TaskState {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(TaskState::parse(&String::deserialize(d)?).unwrap_or_default())
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One section of content: exactly one of `text`, `raw` (base64), `url`,
/// `data` (any JSON value), plus optional `filename` / `mediaType`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Part {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

/// Which content a [`Part`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    Text,
    Raw,
    Url,
    Data,
}

impl Part {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Default::default()
        }
    }

    pub fn data(data: Value) -> Self {
        Self {
            data: Some(data),
            media_type: Some("application/json".into()),
            ..Default::default()
        }
    }

    /// Its content kind; `None` when it holds none or more than one.
    pub fn kind(&self) -> Option<PartKind> {
        let set = [
            (self.text.is_some(), PartKind::Text),
            (self.raw.is_some(), PartKind::Raw),
            (self.url.is_some(), PartKind::Url),
            (self.data.is_some(), PartKind::Data),
        ];
        let mut kinds = set.iter().filter(|(on, _)| *on).map(|(_, k)| *k);
        match (kinds.next(), kinds.next()) {
            (Some(k), None) => Some(k),
            _ => None,
        }
    }
}

/// One unit of communication between client and agent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Message {
    pub message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub role: Role,
    pub parts: Vec<Part>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reference_task_ids: Vec<String>,
}

impl Message {
    /// An agent message holding one text part.
    pub fn agent_text(message_id: String, text: impl Into<String>) -> Self {
        Self {
            message_id,
            role: Role::Agent,
            parts: vec![Part::text(text)],
            ..Default::default()
        }
    }
}

/// Current state of a task, with an optional message and timestamp
/// (ISO 8601 UTC, millisecond precision).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TaskStatus {
    pub state: TaskState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

/// A task output.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Artifact {
    pub artifact_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parts: Vec<Part>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
}

/// The unit of work: status, outputs, history. `artifacts: None` is omitted
/// (`ListTasks` without `includeArtifacts`), `Some([])` is written.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<Artifact>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<Message>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

/// A task's status changed (streaming).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TaskStatusUpdateEvent {
    pub task_id: String,
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

/// A task produced (a chunk of) an artifact (streaming).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TaskArtifactUpdateEvent {
    pub task_id: String,
    pub context_id: String,
    pub artifact: Artifact,
    #[serde(skip_serializing_if = "is_false")]
    pub append: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub last_chunk: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

/// `SendMessage` result: `{"task": …}` or `{"message": …}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SendResult {
    Task(Task),
    Message(Message),
}

/// One event of a stream: `{"task"|"message"|"statusUpdate"|"artifactUpdate": …}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamResponse {
    Task(Task),
    Message(Message),
    StatusUpdate(TaskStatusUpdateEvent),
    ArtifactUpdate(TaskArtifactUpdateEvent),
}

/// `SendMessageRequest.configuration`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SendMessageConfiguration {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub accepted_output_modes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_push_notification_config: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<i64>,
    #[serde(skip_serializing_if = "is_false")]
    pub return_immediately: bool,
}

/// `SendMessage` / `SendStreamingMessage` params.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SendMessageRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub message: Message,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<SendMessageConfiguration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

/// `GetTask` params.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GetTaskRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<i64>,
}

/// `CancelTask` / `SubscribeToTask` params.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TaskIdRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
}

/// `ListTasks` params. `status` stays a string: the server validates it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ListTasksRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_length: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_timestamp_after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_artifacts: Option<bool>,
}

/// `ListTasks` result; every field always written (`nextPageToken` `""` on
/// the last page).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ListTasksResponse {
    pub tasks: Vec<Task>,
    pub next_page_token: String,
    pub page_size: i64,
    pub total_size: i64,
}

/// One way to reach an agent: URL + binding + protocol version.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentInterface {
    pub url: String,
    pub protocol_binding: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub protocol_version: String,
}

/// A 0.3 `additionalInterfaces` entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LegacyInterface {
    pub url: String,
    pub transport: String,
}

/// The agent's provider.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentProvider {
    pub url: String,
    pub organization: String,
}

/// Optional capabilities (`stateTransitionHistory` is 0.3 only).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_notifications: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extended_agent_card: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_transition_history: Option<bool>,
}

/// A distinct ability of the agent (descriptive).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub input_modes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub output_modes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security_requirements: Option<Vec<Value>>,
}

/// The agent's manifest (`/.well-known/agent-card.json`). Security schemes,
/// requirements, signatures and extensions stay JSON: tengu reads only the
/// scheme names it reports and writes only a bearer scheme (`card.rs`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub supported_interfaces: Vec<AgentInterface>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProvider>,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
    pub capabilities: AgentCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security_schemes: Option<BTreeMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security_requirements: Option<Vec<Value>>,
    pub default_input_modes: Vec<String>,
    pub default_output_modes: Vec<String>,
    pub skills: Vec<AgentSkill>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signatures: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    // ── 0.3 fields ──────────────────────────────────────────────────────
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_transport: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_interfaces: Option<Vec<LegacyInterface>>,
}

/// The dialect a JSON-RPC exchange speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// v1.0: PascalCase methods, the types of this file.
    V1,
    /// 0.3: `message/send` …, `kind`-tagged objects (`v03.rs`).
    V03,
}

impl Dialect {
    /// `A2A-Version` value this dialect sends.
    pub fn version(self) -> &'static str {
        match self {
            Dialect::V1 => PROTOCOL_VERSION,
            Dialect::V03 => LEGACY_VERSION,
        }
    }
}

/// Wire binding a client call uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    JsonRpc,
    HttpJson,
}

/// Where and how a client reaches an agent (picked from its card).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub binding: Binding,
    pub dialect: Dialect,
    pub tenant: Option<String>,
}

/// `"1.0"`, `"1.0.1"`, `"1"` → `(1, 0)`; `"0.3.0"` → `(0, 3)`.
pub fn major_minor(version: &str) -> Option<(u32, u32)> {
    let mut it = version.trim().split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().map_or(Some(0), |m| m.parse().ok())?;
    Some((major, minor))
}

impl AgentCard {
    /// The interface a tengu client uses: the first `supportedInterfaces`
    /// entry it speaks (JSON-RPC 1.x or 0.3, HTTP+JSON 1.x — spec § 8.3.2:
    /// earlier entries preferred), else the 0.3 `url` + `preferredTransport`
    /// (JSON-RPC, or none named) or a 0.3 `additionalInterfaces` JSON-RPC
    /// entry. `Err` names what the card offers.
    pub fn endpoint(&self) -> Result<Endpoint, String> {
        for i in &self.supported_interfaces {
            let binding = if i.protocol_binding.eq_ignore_ascii_case(JSONRPC) {
                Binding::JsonRpc
            } else if i.protocol_binding.eq_ignore_ascii_case(HTTP_JSON) {
                Binding::HttpJson
            } else {
                continue;
            };
            let dialect = match major_minor(&i.protocol_version) {
                Some((1, _)) => Dialect::V1,
                Some((0, 3)) if binding == Binding::JsonRpc => Dialect::V03,
                _ => continue,
            };
            if i.url.trim().is_empty() {
                continue;
            }
            return Ok(Endpoint {
                url: i.url.clone(),
                binding,
                dialect,
                tenant: i.tenant.clone().filter(|t| !t.is_empty()),
            });
        }
        let legacy_jsonrpc = self
            .preferred_transport
            .as_deref()
            .map_or(true, |t| t.eq_ignore_ascii_case(JSONRPC));
        if let Some(url) = self.url.as_ref().filter(|u| !u.trim().is_empty()) {
            if legacy_jsonrpc {
                return Ok(Endpoint {
                    url: url.clone(),
                    binding: Binding::JsonRpc,
                    dialect: Dialect::V03,
                    tenant: None,
                });
            }
        }
        if let Some(i) = self
            .additional_interfaces
            .iter()
            .flatten()
            .find(|i| i.transport.eq_ignore_ascii_case(JSONRPC) && !i.url.trim().is_empty())
        {
            return Ok(Endpoint {
                url: i.url.clone(),
                binding: Binding::JsonRpc,
                dialect: Dialect::V03,
                tenant: None,
            });
        }
        let offered: Vec<String> = self
            .supported_interfaces
            .iter()
            .map(|i| format!("{} {} at {}", i.protocol_binding, i.protocol_version, i.url))
            .chain(
                self.preferred_transport
                    .iter()
                    .map(|t| format!("{t} (0.3) at {}", self.url.as_deref().unwrap_or("?"))),
            )
            .collect();
        Err(format!(
            "the card offers no interface tengu speaks (JSON-RPC 1.x / 0.3, HTTP+JSON 1.x): {}",
            if offered.is_empty() {
                "none listed".to_string()
            } else {
                offered.join("; ")
            }
        ))
    }

    /// Names of the security schemes the card requires (v1
    /// `securityRequirements`), empty when none.
    pub fn required_schemes(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .security_requirements
            .iter()
            .flatten()
            .filter_map(|r| r.get("schemes").and_then(Value::as_object))
            .flat_map(|m| m.keys().cloned())
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// Text of the `text` parts, then each `data` part as compact JSON, then a
/// line per file part (name, media type, url or byte count) — what a reader
/// (a model, the operator) gets from parts.
pub fn parts_text(parts: &[Part]) -> String {
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        match p.kind() {
            Some(PartKind::Text) => out.push(p.text.clone().unwrap_or_default()),
            Some(PartKind::Data) => out.push(
                serde_json::to_string(p.data.as_ref().unwrap_or(&Value::Null)).unwrap_or_default(),
            ),
            Some(PartKind::Url) => out.push(format!(
                "[file {} {} {}]",
                p.filename.as_deref().unwrap_or("-"),
                p.media_type.as_deref().unwrap_or("-"),
                p.url.as_deref().unwrap_or("")
            )),
            Some(PartKind::Raw) => out.push(format!(
                "[file {} {} {} base64 chars]",
                p.filename.as_deref().unwrap_or("-"),
                p.media_type.as_deref().unwrap_or("-"),
                p.raw.as_ref().map_or(0, String::len)
            )),
            None => {}
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn task_round_trips_in_v1_form() {
        let task = Task {
            id: "task-1".into(),
            context_id: "ctx-1".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: Some("2026-10-10T10:00:00.000Z".into()),
            },
            artifacts: Some(vec![Artifact {
                artifact_id: "a-1".into(),
                name: Some("response".into()),
                parts: vec![Part::text("hi")],
                ..Default::default()
            }]),
            history: None,
            metadata: None,
        };
        let v = serde_json::to_value(&task).unwrap();
        assert_eq!(
            v,
            json!({"id": "task-1", "contextId": "ctx-1",
                   "status": {"state": "TASK_STATE_COMPLETED", "timestamp": "2026-10-10T10:00:00.000Z"},
                   "artifacts": [{"artifactId": "a-1", "name": "response", "parts": [{"text": "hi"}]}]})
        );
        assert_eq!(serde_json::from_value::<Task>(v).unwrap(), task);
    }

    #[test]
    fn results_are_one_key_objects() {
        let r = SendResult::Message(Message::agent_text("m".into(), "x"));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["message"]["role"], "ROLE_AGENT");
        let s = StreamResponse::StatusUpdate(TaskStatusUpdateEvent::default());
        assert!(serde_json::to_value(&s)
            .unwrap()
            .get("statusUpdate")
            .is_some());
    }

    #[test]
    fn states_and_roles_read_either_spelling() {
        assert_eq!(
            TaskState::parse("input-required"),
            Some(TaskState::InputRequired)
        );
        assert_eq!(
            TaskState::parse("TASK_STATE_CANCELED"),
            Some(TaskState::Canceled)
        );
        assert_eq!(TaskState::parse("TASK_STATE_RUNNING"), None);
        let m: Message =
            serde_json::from_value(json!({"messageId": "1", "role": "user", "parts": []})).unwrap();
        assert_eq!(m.role, Role::User);
        assert!(TaskState::InputRequired.is_settled());
        assert!(!TaskState::Working.is_settled());
    }

    #[test]
    fn a_part_holds_exactly_one_content() {
        assert_eq!(Part::text("a").kind(), Some(PartKind::Text));
        let both = Part {
            text: Some("a".into()),
            url: Some("https://x".into()),
            ..Default::default()
        };
        assert_eq!(both.kind(), None);
        assert_eq!(Part::default().kind(), None);
    }

    #[test]
    fn endpoint_prefers_the_first_spoken_interface() {
        let card: AgentCard = serde_json::from_value(json!({
            "name": "x", "description": "d", "version": "1",
            "supportedInterfaces": [
                {"url": "grpc.example.com:443", "protocolBinding": "GRPC", "protocolVersion": "1.0"},
                {"url": "https://a.example.com/rpc", "protocolBinding": "JSONRPC", "protocolVersion": "1.0", "tenant": "t1"},
                {"url": "https://a.example.com/v03", "protocolBinding": "JSONRPC", "protocolVersion": "0.3"}
            ]
        }))
        .unwrap();
        let e = card.endpoint().unwrap();
        assert_eq!(e.url, "https://a.example.com/rpc");
        assert_eq!(e.dialect, Dialect::V1);
        assert_eq!(e.tenant.as_deref(), Some("t1"));
    }

    #[test]
    fn a_v03_card_is_reached_through_its_url() {
        let card: AgentCard = serde_json::from_value(json!({
            "name": "old", "description": "d", "version": "1", "protocolVersion": "0.3.0",
            "url": "https://old.example.com/a2a", "preferredTransport": "JSONRPC",
            "capabilities": {}, "defaultInputModes": ["text/plain"],
            "defaultOutputModes": ["text/plain"], "skills": []
        }))
        .unwrap();
        let e = card.endpoint().unwrap();
        assert_eq!(
            (e.url.as_str(), e.dialect, e.binding),
            (
                "https://old.example.com/a2a",
                Dialect::V03,
                Binding::JsonRpc
            )
        );
        let grpc_only: AgentCard = serde_json::from_value(json!({
            "name": "g", "description": "d", "version": "1",
            "supportedInterfaces": [{"url": "g:443", "protocolBinding": "GRPC", "protocolVersion": "1.0"}]
        }))
        .unwrap();
        assert!(grpc_only
            .endpoint()
            .unwrap_err()
            .contains("GRPC 1.0 at g:443"));
    }

    #[test]
    fn versions_parse_major_minor() {
        assert_eq!(major_minor("1.0"), Some((1, 0)));
        assert_eq!(major_minor("1.0.1"), Some((1, 0)));
        assert_eq!(major_minor("1"), Some((1, 0)));
        assert_eq!(major_minor("0.3.0"), Some((0, 3)));
        assert_eq!(major_minor("x"), None);
    }

    #[test]
    fn parts_text_reads_every_kind() {
        let parts = vec![
            Part::text("hello"),
            Part::data(json!({"a": 1})),
            Part {
                url: Some("https://f/x.pdf".into()),
                filename: Some("x.pdf".into()),
                media_type: Some("application/pdf".into()),
                ..Default::default()
            },
        ];
        assert_eq!(
            parts_text(&parts),
            "hello\n{\"a\":1}\n[file x.pdf application/pdf https://f/x.pdf]"
        );
    }
}
