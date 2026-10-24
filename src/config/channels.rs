//! Channel sections: `[telegram]`, `[webhooks]` (+ endpoints) and the
//! workspace `[scaffold]` `tengu telegram` applies.
//!
//! | `[telegram]` key | Default | Notes |
//! |---|---|---|
//! | `enabled` | `false` | |
//! | `allowed_users` | `[]` | merged with `TENGU_TELEGRAM_ALLOWED_USERS`; both empty = `tengu telegram` refuses to start; an unlisted sender gets "Unauthorized." |
//! | `tool_approvals` · `approve_only` | — | **removed** 2026-10-23 (they gated nothing): `[telegram]` refuses unknown keys, so either fails the load. No tool call waits for an approval; gate tools with `tools` and scopes |
//!
//! | `[webhooks]` key | Default | Notes |
//! |---|---|---|
//! | `enabled` | `false` | `tengu webhooks` refuses to start when off; `tengu run` mounts the routes only when on (and built with `webhooks`) |
//! | `bind` · `port` | `127.0.0.1` · 7080 | |
//! | `endpoints.<n>.agent` | `""` | informational — the planner routes; required unless `loop` is set |
//! | `endpoints.<n>.loop` | none | feed the payload to `[decision_loops.<loop>]` instead of the planner |
//! | `endpoints.<n>.secret_env` · `.secret` | none | HMAC-SHA256 key (`X-Tengu-Signature: sha256=<hex>`): env var name, or inline (less safe); exactly one |
//! | `endpoints.<n>.auth_header_env` | none | static `Authorization` header value (Helius), constant-time compare; never with an HMAC secret |
//! | `endpoints.<n>.goal_template` | `"A webhook arrived. Process the payload below."` | prefix of the synthesized user message |
//!
//! | Section | Fields (default) | Notes |
//! |---|---|---|
//! | `[scaffold]` | `root` (required), `directories` (`[]`), `files` (`[{path, content}]`), `project.directories` · `project.files` | created when `tengu telegram` starts (files never overwritten); `project` = template of Telegram's `/project <name>`; soft `tengu prune --sandbox` clears `project.directories` |
//!
//! Private agents (no `description`, not `default`) are never `@`-routable from Telegram nor its default.
//!
//! Webhooks, checked when the listener starts: exactly one auth scheme, `loop` names a `[decision_loops]` block, `agent` set when there is no `loop`; an env var missing at request time answers 500. Reply `202 {"session_id": "webhook-<n>-<uuid>"}`; a full loop queue answers 429. Operator doc: `docs/webhooks.md`.

use super::*;

/// Telegram bot adapter configuration. Unknown keys fail the load, the old
/// approval keys `tool_approvals` / `approve_only` too: no tool call waits
/// for an approval on any surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Telegram user ids the bot answers, merged with
    /// `TENGU_TELEGRAM_ALLOWED_USERS`. Fail closed: both empty =
    /// `tengu telegram` refuses to start; an unlisted sender is refused.
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

// =====================================================================
// Webhook listener (added 2026-05-09) — `tengu webhooks --sandbox <name>`
// =====================================================================

/// Inbound HTTP webhook listener.
///
/// Bound to the orchestrator: each endpoint is a `(URL path → agent +
/// secret)` triple. Incoming POSTs are verified with HMAC-SHA256 against
/// the endpoint's `secret_env` (the value of the named env var), then
/// dispatched as a one-shot orchestrator turn with a per-request
/// `session_id` of the form `webhook-<endpoint-name>-<uuid>`.
///
/// The listener responds **202 Accepted** with `{"session_id": "..."}`
/// immediately — agents can take minutes, longer than typical webhook
/// timeouts. Final agent output goes to Open Brain (Postgres
/// `agentic_memory`, with `postgres_memory`) and the tracing log; recall
/// it later by querying Postgres for `session_id = webhook-<name>-<uuid>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// Whether the listener is enabled. `tengu webhooks --sandbox <name>`
    /// refuses to start if `enabled = false`. Default: `false` (off).
    #[serde(default)]
    pub enabled: bool,
    /// Bind address. Default `127.0.0.1` — only loopback by default;
    /// set to `0.0.0.0` to expose externally (paired with HMAC auth).
    #[serde(default = "default_webhook_bind")]
    pub bind: String,
    /// TCP port. Default `7080`.
    #[serde(default = "default_webhook_port")]
    pub port: u16,
    /// Per-endpoint configuration. Map of `<endpoint-name> →
    /// WebhookEndpointConfig`. URL surfaces as `/webhooks/<endpoint-name>`.
    #[serde(default)]
    pub endpoints: HashMap<String, WebhookEndpointConfig>,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_webhook_bind(),
            port: default_webhook_port(),
            endpoints: HashMap::new(),
        }
    }
}

/// One inbound webhook endpoint binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookEndpointConfig {
    /// Agent name to dispatch to (an `[agents.<name>]` block with a `description`).
    /// Informational — the planner routes. Required unless `loop` is set.
    #[serde(default)]
    pub agent: String,
    /// Feed the payload to `[decision_loops.<loop>]` instead of the planner.
    #[serde(default, rename = "loop")]
    pub decision_loop: Option<String>,
    /// Name of the env var holding the exact `Authorization` header value the
    /// sender attaches (Helius webhooks' `authHeader`). Constant-time compare,
    /// no body signature. Mutually exclusive with `secret_env` / `secret`.
    #[serde(default)]
    pub auth_header_env: Option<String>,
    /// Name of the environment variable holding the HMAC shared secret.
    /// At verify time the listener reads `std::env::var(secret_env)`;
    /// missing env var → 500 Internal Error (the listener cannot verify
    /// the signature without the secret). Pair the env var with your
    /// secrets manager / `.env` file. Mutually exclusive with `secret`.
    #[serde(default)]
    pub secret_env: Option<String>,
    /// Inline shared secret. Less safe than `secret_env` (the secret
    /// ends up in the TOML on disk). Use only when `secret_env` is
    /// inconvenient (e.g. local dev). Mutually exclusive with `secret_env`.
    #[serde(default)]
    pub secret: Option<String>,
    /// Prefix prepended to the synthesized user message that the
    /// orchestrator receives. The full user message is
    /// `<goal_template>\n\nPayload (raw body, may be JSON):\n<request body>`, the body in
    /// a `domain::untrusted` fence (origin `webhook:<endpoint>`). Defaults to
    /// `"A webhook arrived. Process the payload below."` if omitted.
    #[serde(default = "default_webhook_goal_template")]
    pub goal_template: String,
}

pub(super) fn default_webhook_bind() -> String {
    "127.0.0.1".to_string()
}

pub(super) fn default_webhook_port() -> u16 {
    7080
}

pub(super) fn default_webhook_goal_template() -> String {
    "A webhook arrived. Process the payload below.".to_string()
}

/// Workspace scaffold — auto-creates directories and seed files on startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScaffoldConfig {
    pub root: String,
    #[serde(default)]
    pub directories: Vec<String>,
    #[serde(default)]
    pub files: Vec<ScaffoldFile>,
    #[serde(default)]
    pub project: Option<ProjectScaffold>,
}

/// Template for per-project scaffolding (applied by `/project <name>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectScaffold {
    #[serde(default)]
    pub directories: Vec<String>,
    #[serde(default)]
    pub files: Vec<ScaffoldFile>,
}

/// A file to seed into the workspace during scaffold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScaffoldFile {
    pub path: String,
    pub content: String,
}
