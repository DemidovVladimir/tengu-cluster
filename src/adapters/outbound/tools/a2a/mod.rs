//! `a2a` — talk to another agent harness over A2A (`domain/a2a/`): an opt-in
//! tool (`tools = ["a2a"]`) that reaches only the remotes of
//! `[a2a.remotes.<name>]` (`config/a2a.rs`), through `outbound/a2a/`.
//!
//! | `action` | Args | Does | Text |
//! |---|---|---|---|
//! | `list` | — | the configured remotes (no request) | one line per remote: name, URL, description |
//! | `card` | `remote` | reads its Agent Card | `render::card`: name, interface used, capabilities, auth, skills |
//! | `send` | `remote`, `message` and / or `data` (a JSON string → a data part), `task_id` + `context_id` to continue, `wait_secs` | `SendMessage` (`returnImmediately`), then waits for the task to settle | `render::send_result`: the task (state, artifacts, `next:`) or the reply message |
//! | `get` | `remote`, `task_id`, `wait_secs` | `GetTask`, waiting when asked | `render::task` |
//! | `cancel` | `remote`, `task_id` | `CancelTask` | `render::task` |
//!
//! `wait_secs` defaults to — and is capped at — the remote's
//! `timeout_secs`; `0` returns the first answer. Unknown keys are refused.
//! Cards are kept 5 minutes per tool instance. Scope: `net_hosts` (the
//! remote's host, every request), `env_reads` (its credential variable);
//! no file system. Engines: in-process (OpenRouter, local) and the MCP
//! bridge (Claude Code, Codex) run the same code; the result is text,
//! capped at the remote's `max_result_chars` with ids whole.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::outbound::a2a::A2aClient;
use crate::config::a2a::{A2aConfig, A2aRemoteConfig};
use crate::config::sections::SandboxSections;
use crate::domain::a2a::model::{AgentCard, Endpoint, Message, Part, Role, SendMessageRequest};
use crate::domain::a2a::render;
use crate::domain::message::ToolDef;
use crate::domain::tools::A2A;
use crate::ports::tool::{PluginCtx, Tool, ToolCtx, ToolOutput, ToolPlugin};

/// How long a fetched card is reused.
const CARD_TTL: Duration = Duration::from_secs(300);
const ARGS: [&str; 7] = [
    "action",
    "remote",
    "message",
    "data",
    "task_id",
    "context_id",
    "wait_secs",
];

/// The tool's definition (catalog row).
pub(crate) fn tool_defs() -> Vec<ToolDef> {
    vec![definition()]
}

fn definition() -> ToolDef {
    ToolDef::new(
        A2A,
        "Talk to another agent harness over the A2A protocol (Agent2Agent). Only the remotes \
         configured for this sandbox are reachable — call action \"list\" first. \"card\" shows \
         what a remote does; \"send\" sends it a message and waits for the answer; when the \
         answer says it needs more input, \"send\" again with the same task_id and context_id. \
         \"get\" re-reads a task (wait_secs waits for it to finish), \"cancel\" stops one.",
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "card", "send", "get", "cancel"],
                    "description": "list the remotes, read a remote's card, send a message, get or cancel a task"
                },
                "remote": {
                    "type": "string",
                    "description": "Name of a configured remote (from action list). Required for every action but list."
                },
                "message": {
                    "type": "string",
                    "description": "send: the text to send to the remote agent."
                },
                "data": {
                    "type": "string",
                    "description": "send: optional JSON (as a string) sent as a structured data part."
                },
                "task_id": {
                    "type": "string",
                    "description": "send: continue this task (it asked for more input); get / cancel: the task."
                },
                "context_id": {
                    "type": "string",
                    "description": "send: continue this conversation (the context_id of an earlier answer)."
                },
                "wait_secs": {
                    "type": "integer",
                    "description": "send / get: seconds to wait for the task to finish (default and maximum: the remote's timeout; 0 = do not wait)."
                }
            },
            "required": ["action"]
        }),
    )
}

/// One parsed call.
#[derive(Debug, Default, PartialEq)]
struct Args {
    action: String,
    remote: Option<String>,
    message: Option<String>,
    data: Option<Value>,
    task_id: Option<String>,
    context_id: Option<String>,
    wait_secs: Option<u64>,
}

impl Args {
    fn parse(v: &Value) -> Result<Self> {
        let obj = v
            .as_object()
            .ok_or_else(|| anyhow!("a2a: arguments must be an object"))?;
        if let Some(k) = obj.keys().find(|k| !ARGS.contains(&k.as_str())) {
            bail!("a2a: unknown argument `{k}` (known: {})", ARGS.join(", "));
        }
        let text = |k: &str| -> Result<Option<String>> {
            match obj.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                Some(other) => bail!("a2a: `{k}` must be a string, got {other}"),
            }
        };
        let data = match obj.get("data") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.trim().is_empty() => None,
            Some(Value::String(s)) => Some(
                serde_json::from_str(s)
                    .map_err(|e| anyhow!("a2a: `data` must be JSON text: {e}"))?,
            ),
            // A model that passes the JSON itself.
            Some(other) => Some(other.clone()),
        };
        let wait_secs = match obj.get("wait_secs") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                    .ok_or_else(|| anyhow!("a2a: `wait_secs` must be a whole number ≥ 0"))?,
            ),
        };
        let action = text("action")?.ok_or_else(|| anyhow!("a2a: `action` is required"))?;
        if !["list", "card", "send", "get", "cancel"].contains(&action.as_str()) {
            bail!("a2a: unknown action `{action}` (list, card, send, get, cancel)");
        }
        Ok(Self {
            action,
            remote: text("remote")?,
            message: text("message")?,
            data,
            task_id: text("task_id")?,
            context_id: text("context_id")?,
            wait_secs,
        })
    }
}

/// The `a2a` tool of one agent.
pub(crate) struct A2aTool {
    def: ToolDef,
    sandbox: Arc<SandboxSections>,
    cards: Mutex<HashMap<String, (AgentCard, Instant)>>,
}

impl A2aTool {
    pub(crate) fn new(sandbox: Arc<SandboxSections>) -> Self {
        Self {
            def: definition(),
            sandbox,
            cards: Mutex::new(HashMap::new()),
        }
    }

    fn config(&self) -> Option<&A2aConfig> {
        self.sandbox.a2a.as_deref()
    }

    /// The configured remote `name`, or a refusal naming the ones there are.
    fn remote(&self, name: Option<&str>) -> Result<(String, A2aRemoteConfig)> {
        let names = || -> String {
            let n: Vec<&str> = self
                .config()
                .map(|c| c.remotes.keys().map(String::as_str).collect())
                .unwrap_or_default();
            if n.is_empty() {
                "none — this sandbox has no [a2a.remotes.<name>]".into()
            } else {
                n.join(", ")
            }
        };
        let name =
            name.ok_or_else(|| anyhow!("a2a: `remote` is required (remotes: {})", names()))?;
        self.config()
            .and_then(|c| c.remotes.get(name))
            .map(|r| (name.to_string(), r.clone()))
            .ok_or_else(|| anyhow!("a2a: no remote `{name}` (remotes: {})", names()))
    }

    fn list(&self) -> String {
        let Some(c) = self.config().filter(|c| !c.remotes.is_empty()) else {
            return "a2a: no remotes — this sandbox has no [a2a.remotes.<name>]".into();
        };
        let mut out = format!("a2a remotes ({}):", c.remotes.len());
        for (name, r) in &c.remotes {
            out.push_str(&format!("\n- {name}  {}", r.url));
            if let Some(d) = &r.description {
                out.push_str(&format!("  — {d}"));
            }
        }
        out.push_str("\nnext: card to see what one does, send to talk to it");
        out
    }

    /// The remote's card (cached) and interface.
    async fn connect(&self, client: &A2aClient<'_>) -> Result<(AgentCard, Endpoint)> {
        let cached = self
            .cards
            .lock()
            .ok()
            .and_then(|m| m.get(client.name()).cloned())
            .filter(|(_, at)| at.elapsed() < CARD_TTL)
            .map(|(c, _)| c);
        let card = match cached {
            Some(c) => c,
            None => {
                let c = client.card().await?;
                if let Ok(mut m) = self.cards.lock() {
                    m.insert(client.name().to_string(), (c.clone(), Instant::now()));
                }
                c
            }
        };
        let ep = client.endpoint(&card)?;
        Ok((card, ep))
    }
}

#[async_trait]
impl Tool for A2aTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let a = Args::parse(args)?;
        if a.action == "list" {
            // No request, no env read: the sandbox's own config.
            return Ok(self.list().into());
        }
        let (name, cfg) = self.remote(a.remote.as_deref())?;
        let host = reqwest::Url::parse(&cfg.url)?
            .host_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        // The remote's host; every request re-checks its own (outbound/a2a).
        ctx.scope.check_net_host(&host)?;
        let wait = a
            .wait_secs
            .unwrap_or(cfg.timeout_secs)
            .min(cfg.timeout_secs);
        let role = ctx.agent_config.and_then(|c| c.role.clone());
        let client = A2aClient::new(
            &name,
            &cfg,
            Some(ctx.scope),
            ctx.secret_registry,
            role,
            Duration::from_secs(wait.max(5)),
        )?;
        let (card, ep) = self.connect(&client).await?;
        let max = cfg.max_result_chars;
        let text = match a.action.as_str() {
            "card" => render::card(&name, &card),
            "send" => {
                let req = send_request(&a, ctx.call_id)?;
                let r = client
                    .send_and_wait(&ep, &req, Duration::from_secs(wait))
                    .await?;
                render::send_result(&name, &r, max)
            }
            "get" => {
                let id = task_id(&a)?;
                let t = client
                    .get_and_wait(
                        &ep,
                        id,
                        Duration::from_secs(a.wait_secs.unwrap_or(0).min(wait)),
                    )
                    .await?;
                render::task(&name, &t, max)
            }
            _ => {
                let t = client.cancel(&ep, task_id(&a)?).await?;
                render::task(&name, &t, max)
            }
        };
        Ok(text.into())
    }
}

fn task_id(a: &Args) -> Result<&str> {
    a.task_id
        .as_deref()
        .ok_or_else(|| anyhow!("a2a: `task_id` is required for {}", a.action))
}

/// The message of a `send`: a text part and / or a data part. Its
/// `messageId` = the call id (a retry of the call is the same message; the
/// spec lets an agent drop duplicates), else a fresh UUID.
fn send_request(a: &Args, call_id: Option<&str>) -> Result<SendMessageRequest> {
    let mut parts = Vec::new();
    if let Some(m) = &a.message {
        parts.push(Part::text(m));
    }
    if let Some(d) = &a.data {
        parts.push(Part::data(d.clone()));
    }
    if parts.is_empty() {
        bail!("a2a: send needs `message` (text) and / or `data` (JSON)");
    }
    Ok(SendMessageRequest {
        message: Message {
            message_id: call_id
                .filter(|c| !c.is_empty())
                .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string),
            context_id: a.context_id.clone(),
            task_id: a.task_id.clone(),
            role: Role::User,
            parts,
            ..Default::default()
        },
        ..Default::default()
    })
}

/// Plugin of the `a2a` tool.
pub(crate) struct A2aPlugin;

#[async_trait]
impl ToolPlugin for A2aPlugin {
    fn name(&self) -> &'static str {
        "a2a"
    }

    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        Ok(vec![Arc::new(A2aTool::new(Arc::clone(
            &ctx.config.sandbox,
        )))])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::a2a::tests::{mock, v1_card};
    use crate::domain::scope::ToolScope;
    use crate::domain::secrets::SecretRegistry;

    fn sandbox(remotes: Value) -> Arc<SandboxSections> {
        let cfg: A2aConfig = serde_json::from_value(json!({"remotes": remotes})).unwrap();
        Arc::new(SandboxSections {
            a2a: Some(Arc::new(cfg)),
            ..Default::default()
        })
    }

    struct NoShell;
    impl crate::ports::shell::ShellExecutionPort for NoShell {
        fn execute_shell(&self, _: &str, _: &std::path::Path) -> Result<String> {
            Ok(String::new())
        }
    }

    async fn call(tool: &A2aTool, scope: &ToolScope, args: Value) -> Result<String> {
        let secrets = SecretRegistry::new();
        let shell = NoShell;
        let http = reqwest::Client::new();
        let activity = crate::adapters::outbound::noop::NoopActivity;
        let ws = std::env::temp_dir();
        let ctx = ToolCtx {
            workspace: &ws,
            scope,
            shell: &shell,
            http: &http,
            memory_manager: None,
            secret_registry: &secrets,
            activity: &activity,
            conversation: crate::ports::tool::ConversationView::empty(),
            agent_config: None,
            call_id: None,
        };
        tool.execute(&args, &ctx).await.map(|o| o.text)
    }

    fn open_scope() -> ToolScope {
        ToolScope {
            net_hosts: vec!["127.0.0.1".into()],
            ..Default::default()
        }
    }

    #[test]
    fn args_parse_strictly() {
        let a = Args::parse(&json!({"action": "send", "remote": "r", "message": "hi",
                                    "data": "{\"x\": 1}", "wait_secs": "7"}))
        .unwrap();
        assert_eq!(a.data, Some(json!({"x": 1})));
        assert_eq!(a.wait_secs, Some(7));
        let e = Args::parse(&json!({"action": "send", "url": "x"})).unwrap_err();
        assert!(e.to_string().contains("unknown argument `url`"), "{e}");
        assert!(Args::parse(&json!({"action": "fly"})).is_err());
        assert!(Args::parse(&json!({"action": "send", "data": "{nope"})).is_err());
        assert!(send_request(&Args::parse(&json!({"action": "send"})).unwrap(), None).is_err());
        let a = Args::parse(&json!({"action": "send", "message": "m"})).unwrap();
        assert_eq!(
            send_request(&a, Some("chat:n:0:0:c1"))
                .unwrap()
                .message
                .message_id,
            "chat:n:0:0:c1"
        );
    }

    #[test]
    fn the_schema_passes_the_lint() {
        let v = crate::adapters::outbound::tools::schema_lint::violations(&definition());
        assert!(v.is_empty(), "{v:?}");
    }

    #[tokio::test]
    async fn lists_and_refuses_unknown_remotes() {
        let tool = A2aTool::new(sandbox(json!({"desk": {"url": "https://a.example.com",
                                                        "description": "Research desk."}})));
        let scope = open_scope();
        let text = call(&tool, &scope, json!({"action": "list"}))
            .await
            .unwrap();
        assert!(
            text.contains("- desk  https://a.example.com  — Research desk."),
            "{text}"
        );
        let e = call(&tool, &scope, json!({"action": "card", "remote": "nope"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("no remote `nope` (remotes: desk)"), "{e}");
        let none = A2aTool::new(Arc::new(SandboxSections::default()));
        let text = call(&none, &scope, json!({"action": "list"}))
            .await
            .unwrap();
        assert!(text.contains("no remotes"), "{text}");
    }

    #[tokio::test]
    async fn the_scope_gates_the_remote_host() {
        let tool = A2aTool::new(sandbox(json!({"desk": {"url": "https://a.example.com"}})));
        let e = call(
            &tool,
            &ToolScope::default(),
            json!({"action": "card", "remote": "desk"}),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(e.contains("a.example.com"), "{e}");
    }

    #[tokio::test]
    async fn sends_and_reads_the_answer() {
        let m = mock(
            |_| Some(v1_card("http://127.0.0.1:1/")),
            |_, body| {
                let text = body["params"]["message"]["parts"][0]["text"]
                    .as_str()
                    .unwrap_or("");
                json!({"jsonrpc": "2.0", "id": body["id"], "result": {"task": {
                    "id": "task-7f3e", "contextId": "ctx-91",
                    "status": {"state": "TASK_STATE_COMPLETED"},
                    "artifacts": [{"artifactId": "art-1", "name": "answer",
                                   "parts": [{"text": format!("echo: {text}")}]}]}}})
            },
        );
        let tool = A2aTool::new(sandbox(
            json!({"echo": {"url": m.url, "endpoint_url": m.url}}),
        ));
        let scope = open_scope();
        let card = call(&tool, &scope, json!({"action": "card", "remote": "echo"}))
            .await
            .unwrap();
        assert!(card.starts_with("a2a echo: Mock agent v1.0.0"), "{card}");
        let out = call(
            &tool,
            &scope,
            json!({"action": "send", "remote": "echo", "message": "ping"}),
        )
        .await
        .unwrap();
        assert!(
            out.starts_with("a2a echo: task task-7f3e TASK_STATE_COMPLETED context ctx-91"),
            "{out}"
        );
        assert!(out.contains("artifact answer art-1:\necho: ping"), "{out}");
        // The card was read once (cached).
        let gets = m
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _, _)| m == "GET")
            .count();
        assert_eq!(gets, 1);
    }
}
