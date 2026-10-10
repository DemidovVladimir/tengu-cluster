//! A2A server use case — what `tengu a2a serve` (`inbound/a2a.rs`) does
//! with one JSON-RPC request to a served endpoint, independent of HTTP.
//! Every message becomes a task run by an `A2aRunner` (`ports/a2a.rs`):
//! the planner or one exposed agent, in this process.
//!
//! | Method (v1.0 · 0.3) | Answer |
//! |---|---|
//! | `SendMessage` · `message/send` | a new task (`SUBMITTED` → `WORKING` → `COMPLETED` with one `text/plain` artifact `response`, or `FAILED` with the error as status message); waits for it to settle unless `returnImmediately` (0.3: `blocking: false`) |
//! | `SendStreamingMessage` · `message/stream` | SSE: the task, then `statusUpdate` / `artifactUpdate` events until it settles |
//! | `GetTask` · `tasks/get` | the task (`historyLength` applied) |
//! | `ListTasks` (v1.0) | the endpoint's tasks, newest status first, paged (`tasks.rs`) |
//! | `CancelTask` · `tasks/cancel` | stops the run (its subprocesses die with it), `CANCELED`; a settled task is `TaskNotCancelable` |
//! | `SubscribeToTask` · `tasks/resubscribe` | SSE from the current task on; a settled task is `UnsupportedOperation` |
//! | push notification config methods | `PushNotificationNotSupported` (the card says `pushNotifications: false`) |
//! | `GetExtendedAgentCard` | `UnsupportedOperation` (`extendedAgentCard: false`) |
//!
//! | Message rule | Effect |
//! |---|---|
//! | parts | text, data (fenced JSON), text-like `raw`; a file `url` or binary `raw` ⇒ `ContentTypeNotSupported` |
//! | `contextId` | kept (new ⇒ minted); the context's last `context_turns` completed turns go to the runner; the planner's session id is `a2a-<contextId>` |
//! | `taskId` | must be this endpoint's task; tengu tasks never wait for input, so a settled one ⇒ `UnsupportedOperation` (send in its context instead), a running one ⇒ `UnsupportedOperation` (wait or cancel) |
//! | `acceptedOutputModes` | must allow `text/plain` (or be empty) |
//! | push config in `configuration` | `PushNotificationNotSupported` |
//!
//! Runs: at most `max_running` at once (the rest stay `SUBMITTED`), each
//! cut at `run_timeout` (`FAILED`, "timed out"). Tasks live in memory
//! (`max_tasks`); a restart forgets them.

pub(crate) mod tasks;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, watch, Semaphore};

use crate::domain::a2a::model::{
    Artifact, Dialect, GetTaskRequest, ListTasksRequest, Message, Part, Role, SendMessageRequest,
    StreamResponse, Task, TaskArtifactUpdateEvent, TaskIdRequest, TaskState, TaskStatus,
    TaskStatusUpdateEvent,
};
use crate::domain::a2a::rpc::{self, Method, RpcError};
use crate::domain::a2a::{iso_ms, v03};
use crate::ports::a2a::{A2aRunner, A2aTarget, A2aTurn};
use crate::ports::clock::Clock;
use tasks::{accepts_text, check_history_length, turn_text, with_history, Entry, Store};

/// Server limits (`[a2a.server]`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Limits {
    pub max_tasks: usize,
    pub max_running: usize,
    pub context_turns: usize,
    pub run_timeout: Duration,
}

/// What the HTTP side sends back.
pub(crate) enum Reply {
    /// A JSON-RPC response (result or error).
    Json(Value),
    /// SSE: each event becomes `{"jsonrpc", "id", "result": <event>}` in
    /// `dialect`.
    Stream {
        id: Value,
        dialect: Dialect,
        events: mpsc::Receiver<StreamResponse>,
    },
}

/// One stream event as the JSON-RPC result of its dialect.
pub(crate) fn stream_event(id: &Value, dialect: Dialect, e: &StreamResponse) -> Value {
    let result = match dialect {
        Dialect::V1 => serde_json::to_value(e).unwrap_or(Value::Null),
        Dialect::V03 => v03::stream_to_v03(e),
    };
    rpc::success(id, result)
}

struct Shared {
    runner: Arc<dyn A2aRunner>,
    clock: Arc<dyn Clock>,
    limits: Limits,
    store: Mutex<Store>,
    permits: Arc<Semaphore>,
}

/// The A2A server of one process (module tables).
#[derive(Clone)]
pub(crate) struct A2aService {
    shared: Arc<Shared>,
}

enum Outcome {
    Value(Value),
    Stream(mpsc::Receiver<StreamResponse>),
}

fn parse<T: serde::de::DeserializeOwned>(params: &Value) -> Result<T, RpcError> {
    serde_json::from_value(params.clone())
        .map_err(|e| RpcError::invalid_params("params", e.to_string()))
}

fn task_json(dialect: Dialect, t: &Task) -> Value {
    match dialect {
        Dialect::V1 => serde_json::to_value(t).unwrap_or(Value::Null),
        Dialect::V03 => v03::task_to_v03(t),
    }
}

impl A2aService {
    pub(crate) fn new(runner: Arc<dyn A2aRunner>, clock: Arc<dyn Clock>, limits: Limits) -> Self {
        let permits = Arc::new(Semaphore::new(limits.max_running.max(1)));
        Self {
            shared: Arc::new(Shared {
                runner,
                clock,
                limits,
                store: Mutex::new(Store::default()),
                permits,
            }),
        }
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.shared.store.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Answer one request body sent to `target`'s endpoint with `version`
    /// = its `A2A-Version` header.
    pub(crate) async fn handle(
        &self,
        target: &A2aTarget,
        version: Option<&str>,
        body: &[u8],
    ) -> Reply {
        let req = match rpc::parse_request(body) {
            Ok(r) => r,
            Err((id, e)) => return Reply::Json(rpc::failure(&id, &e)),
        };
        let (method, dialect) = match rpc::negotiate(version, &req.method) {
            Ok(x) => x,
            Err(e) => return Reply::Json(rpc::failure(&req.id, &e)),
        };
        tracing::info!(target: "tengu::a2a", endpoint = %target.label(), method = %req.method, "a2a request");
        match self.dispatch(target, method, dialect, &req.params).await {
            Ok(Outcome::Value(v)) => Reply::Json(rpc::success(&req.id, v)),
            Ok(Outcome::Stream(events)) => Reply::Stream {
                id: req.id,
                dialect,
                events,
            },
            Err(e) => {
                tracing::info!(target: "tengu::a2a", endpoint = %target.label(), method = %req.method, error = %e, "a2a request refused");
                Reply::Json(rpc::failure(&req.id, &e))
            }
        }
    }

    async fn dispatch(
        &self,
        target: &A2aTarget,
        method: Method,
        dialect: Dialect,
        params: &Value,
    ) -> Result<Outcome, RpcError> {
        match method {
            Method::SendMessage | Method::SendStreamingMessage => {
                let req: SendMessageRequest = match dialect {
                    Dialect::V1 => parse(params)?,
                    Dialect::V03 => v03::send_request_from_v03(params),
                };
                let cfg = req.configuration.clone().unwrap_or_default();
                check_history_length(cfg.history_length)?;
                let (rx, created) = self.start(target, &req)?;
                if method == Method::SendStreamingMessage {
                    return Ok(Outcome::Stream(stream(rx, created)));
                }
                let task = if cfg.return_immediately {
                    rx.borrow().clone()
                } else {
                    self.settled(rx).await
                };
                let task = with_history(task, cfg.history_length);
                Ok(Outcome::Value(match dialect {
                    Dialect::V1 => json!({"task": task}),
                    Dialect::V03 => v03::task_to_v03(&task),
                }))
            }
            Method::GetTask => {
                let req: GetTaskRequest = match dialect {
                    Dialect::V1 => parse(params)?,
                    Dialect::V03 => v03::get_request_from_v03(params),
                };
                check_history_length(req.history_length)?;
                let t = self.store().get(target, &req.id)?.task.clone();
                Ok(Outcome::Value(task_json(
                    dialect,
                    &with_history(t, req.history_length),
                )))
            }
            Method::ListTasks => {
                let req: ListTasksRequest = parse(params)?;
                let page = self.store().list(target, &req)?;
                Ok(Outcome::Value(
                    serde_json::to_value(page).unwrap_or(Value::Null),
                ))
            }
            Method::CancelTask => {
                let req: TaskIdRequest = match dialect {
                    Dialect::V1 => parse(params)?,
                    Dialect::V03 => v03::task_id_request_from_v03(params),
                };
                let t = self.cancel(target, &req.id)?;
                Ok(Outcome::Value(task_json(dialect, &t)))
            }
            Method::SubscribeToTask => {
                let req: TaskIdRequest = match dialect {
                    Dialect::V1 => parse(params)?,
                    Dialect::V03 => v03::task_id_request_from_v03(params),
                };
                let store = self.store();
                let e = store.get(target, &req.id)?;
                let state = e.task.status.state;
                if state.is_terminal() {
                    return Err(RpcError::unsupported(format!(
                        "task {} is {} — subscribe works on unfinished tasks; use GetTask",
                        req.id,
                        state.proto_name()
                    )));
                }
                let (rx, now) = (e.tx.subscribe(), e.task.clone());
                drop(store);
                Ok(Outcome::Stream(stream(rx, now)))
            }
            Method::CreatePushConfig
            | Method::GetPushConfig
            | Method::ListPushConfigs
            | Method::DeletePushConfig => Err(RpcError::push_not_supported()),
            Method::GetExtendedAgentCard => Err(RpcError::unsupported(
                "this agent has no extended card (capabilities.extendedAgentCard is false)",
            )),
        }
    }

    /// Validate a message, store its task, start its run; the task's feed
    /// and the task as created (`SUBMITTED`).
    fn start(
        &self,
        target: &A2aTarget,
        req: &SendMessageRequest,
    ) -> Result<(watch::Receiver<Task>, Task), RpcError> {
        let msg = &req.message;
        if msg.message_id.trim().is_empty() {
            return Err(RpcError::invalid_params("message.messageId", "required"));
        }
        if msg.role == Role::Agent {
            return Err(RpcError::invalid_params(
                "message.role",
                "a client sends ROLE_USER messages",
            ));
        }
        let cfg = req.configuration.clone().unwrap_or_default();
        if cfg.task_push_notification_config.is_some() {
            return Err(RpcError::push_not_supported());
        }
        if !accepts_text(&cfg.accepted_output_modes) {
            return Err(RpcError::content_type(format!(
                "this agent answers in text/plain; acceptedOutputModes is {:?}",
                cfg.accepted_output_modes
            )));
        }
        let text = turn_text(&msg.parts)?;
        let now = self.shared.clock.now_ms();
        let mut store = self.store();
        if let Some(tid) = msg.task_id.as_deref().filter(|t| !t.is_empty()) {
            let e = store.get(target, tid)?;
            if let Some(c) = msg
                .context_id
                .as_deref()
                .filter(|c| *c != e.task.context_id)
            {
                return Err(RpcError::invalid_params(
                    "message.contextId",
                    format!(
                        "'{c}' is not the context of task {tid} ('{}')",
                        e.task.context_id
                    ),
                ));
            }
            let state = e.task.status.state;
            return Err(RpcError::unsupported(if state.is_terminal() {
                format!(
                    "task {tid} is {} — it takes no more messages; send without taskId and with contextId {} to continue the conversation",
                    state.proto_name(),
                    e.task.context_id
                )
            } else {
                format!("task {tid} is still running — wait for it (GetTask) or cancel it")
            }));
        }
        store.make_room(self.shared.limits.max_tasks)?;
        let context_id = msg
            .context_id
            .clone()
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let task_id = uuid::Uuid::new_v4().to_string();
        let mut user = msg.clone();
        user.task_id = Some(task_id.clone());
        user.context_id = Some(context_id.clone());
        user.role = Role::User;
        let task = Task {
            id: task_id.clone(),
            context_id: context_id.clone(),
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: Some(iso_ms(now)),
            },
            artifacts: Some(Vec::new()),
            history: Some(vec![user]),
            metadata: None,
        };
        let created = task.clone();
        let (tx, rx) = watch::channel(task.clone());
        let turn = A2aTurn {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            text,
            history: store.history(target, &context_id),
        };
        store.tasks.insert(
            task_id.clone(),
            Entry {
                target: target.clone(),
                task,
                updated_ms: now,
                tx,
                abort: None,
            },
        );
        let shared = Arc::clone(&self.shared);
        let run_target = target.clone();
        let handle = tokio::spawn(async move { run(shared, run_target, turn).await });
        if let Some(e) = store.tasks.get_mut(&task_id) {
            e.abort = Some(handle.abort_handle());
        }
        tracing::info!(target: "tengu::a2a", endpoint = %target.label(), task = %task_id, context = %context_id, "a2a task started");
        Ok((rx, created))
    }

    /// Wait until the task settles (a run cut at `run_timeout` settles too).
    async fn settled(&self, mut rx: watch::Receiver<Task>) -> Task {
        let cap = self.shared.limits.run_timeout + Duration::from_secs(30);
        let _ = tokio::time::timeout(cap, rx.wait_for(|t| t.status.state.is_settled())).await;
        let t = rx.borrow().clone();
        t
    }

    /// `CancelTask` (module table).
    fn cancel(&self, target: &A2aTarget, id: &str) -> Result<Task, RpcError> {
        let now = self.shared.clock.now_ms();
        let mut store = self.store();
        let e = store.get(target, id)?;
        let state = e.task.status.state;
        if state.is_terminal() {
            return Err(RpcError::not_cancelable(id, state.proto_name()));
        }
        if let Some(a) = &e.abort {
            a.abort();
        }
        store.update(id, now, |t| {
            t.status.state = TaskState::Canceled;
            t.status.message = Some(Message::agent_text(
                uuid::Uuid::new_v4().to_string(),
                "canceled by the client",
            ));
        });
        tracing::info!(target: "tengu::a2a", endpoint = %target.label(), task = %id, "a2a task canceled");
        Ok(store.get(target, id)?.task.clone())
    }

    /// Stop every unsettled run (shutdown): `FAILED`, "server stopped".
    pub(crate) fn stop_all(&self) {
        let now = self.shared.clock.now_ms();
        let mut store = self.store();
        let open: Vec<String> = store
            .tasks
            .iter()
            .filter(|(_, e)| !e.task.status.state.is_settled())
            .map(|(id, _)| id.clone())
            .collect();
        for id in open {
            if let Some(a) = store.tasks.get(&id).and_then(|e| e.abort.clone()) {
                a.abort();
            }
            store.update(&id, now, |t| {
                t.status.state = TaskState::Failed;
                t.status.message = Some(Message::agent_text(
                    uuid::Uuid::new_v4().to_string(),
                    "the tengu A2A server stopped before the task finished",
                ));
            });
        }
    }
}

/// The run of one task (module table: runs).
async fn run(shared: Arc<Shared>, target: A2aTarget, turn: A2aTurn) {
    let Ok(_permit) = Arc::clone(&shared.permits).acquire_owned().await else {
        return;
    };
    let id = turn.task_id.clone();
    {
        let now = shared.clock.now_ms();
        let mut store = shared.store.lock().unwrap_or_else(|p| p.into_inner());
        if !store.update(&id, now, |t| t.status.state = TaskState::Working) {
            return; // canceled while queued
        }
    }
    let secs = shared.limits.run_timeout.as_secs();
    let outcome = tokio::time::timeout(
        shared.limits.run_timeout,
        shared.runner.run(&target, turn.clone()),
    )
    .await;
    let now = shared.clock.now_ms();
    let mut store = shared.store.lock().unwrap_or_else(|p| p.into_inner());
    let (state, text) = match outcome {
        Ok(Ok(answer)) => {
            let answer = if answer.trim().is_empty() {
                "(the agent returned no text)".to_string()
            } else {
                answer
            };
            let artifact = Artifact {
                artifact_id: uuid::Uuid::new_v4().to_string(),
                name: Some("response".into()),
                description: None,
                parts: vec![Part {
                    media_type: Some("text/plain".into()),
                    ..Part::text(answer.clone())
                }],
                metadata: None,
                extensions: Vec::new(),
            };
            let done = store.update(&id, now, |t| {
                t.status.state = TaskState::Completed;
                t.status.message = None;
                t.artifacts.get_or_insert_with(Vec::new).push(artifact);
            });
            if done {
                let limit = shared.limits.context_turns;
                store.remember(
                    &target,
                    &turn.context_id,
                    (turn.text.clone(), answer),
                    limit,
                );
            }
            (TaskState::Completed, None)
        }
        Ok(Err(e)) => (TaskState::Failed, Some(format!("{e:#}"))),
        Err(_) => (TaskState::Failed, Some(format!("timed out after {secs} s"))),
    };
    if let Some(why) = &text {
        store.update(&id, now, |t| {
            t.status.state = TaskState::Failed;
            t.status.message = Some(Message::agent_text(
                uuid::Uuid::new_v4().to_string(),
                why.clone(),
            ));
        });
    }
    tracing::info!(target: "tengu::a2a", endpoint = %target.label(), task = %id, state = %state.proto_name(), error = ?text, "a2a task settled");
}

/// SSE feed of one task: `first` (the task as created, or as it was at
/// subscribe time), then each artifact it gained (`artifactUpdate`, whole:
/// `lastChunk`) and each state change (`statusUpdate`) — read from the
/// feed's latest value, so a run that settles before the stream starts is
/// still replayed — closing once it settles.
fn stream(mut rx: watch::Receiver<Task>, first: Task) -> mpsc::Receiver<StreamResponse> {
    let (tx, out) = mpsc::channel(32);
    tokio::spawn(async move {
        let mut state = first.status.state;
        let mut sent = first.artifacts.as_ref().map_or(0, Vec::len);
        if tx.send(StreamResponse::Task(first)).await.is_err() || state.is_settled() {
            return;
        }
        loop {
            let t = rx.borrow_and_update().clone();
            for a in t.artifacts.iter().flatten().skip(sent) {
                let ev = StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                    task_id: t.id.clone(),
                    context_id: t.context_id.clone(),
                    artifact: a.clone(),
                    append: false,
                    last_chunk: true,
                    metadata: None,
                });
                if tx.send(ev).await.is_err() {
                    return;
                }
            }
            sent = sent.max(t.artifacts.as_ref().map_or(0, Vec::len));
            if t.status.state != state {
                state = t.status.state;
                let ev = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id: t.id.clone(),
                    context_id: t.context_id.clone(),
                    status: t.status.clone(),
                    metadata: None,
                });
                if tx.send(ev).await.is_err() {
                    return;
                }
            }
            if state.is_settled() || rx.changed().await.is_err() {
                return;
            }
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::clock::SimClock;
    use async_trait::async_trait;

    /// Answers `echo: <text>`; `fail` fails; `slow` waits a minute;
    /// records each turn.
    struct Echo {
        turns: Mutex<Vec<A2aTurn>>,
    }

    #[async_trait]
    impl A2aRunner for Echo {
        async fn run(&self, target: &A2aTarget, turn: A2aTurn) -> anyhow::Result<String> {
            self.turns.lock().unwrap().push(turn.clone());
            match turn.text.as_str() {
                "fail" => anyhow::bail!("the agent broke"),
                "slow" => {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Ok("late".into())
                }
                t => Ok(format!("{} echo: {t}", target.label())),
            }
        }
    }

    fn service() -> (A2aService, Arc<Echo>) {
        let echo = Arc::new(Echo {
            turns: Mutex::new(Vec::new()),
        });
        let svc = A2aService::new(
            echo.clone(),
            Arc::new(SimClock::at(1_760_090_400_000)),
            Limits {
                max_tasks: 50,
                max_running: 4,
                context_turns: 8,
                run_timeout: Duration::from_secs(5),
            },
        );
        (svc, echo)
    }

    async fn call(
        svc: &A2aService,
        target: &A2aTarget,
        version: Option<&str>,
        body: Value,
    ) -> Value {
        match svc
            .handle(target, version, body.to_string().as_bytes())
            .await
        {
            Reply::Json(v) => v,
            Reply::Stream { .. } => panic!("a stream"),
        }
    }

    fn send(text: &str, extra: Value) -> Value {
        let mut msg = json!({"messageId": "m-1", "role": "ROLE_USER", "parts": [{"text": text}]});
        if let Value::Object(e) = extra {
            for (k, v) in e {
                msg[k] = v;
            }
        }
        json!({"jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": {"message": msg}})
    }

    #[tokio::test]
    async fn a_blocking_send_completes_with_one_artifact() {
        let (svc, _) = service();
        let v = call(
            &svc,
            &A2aTarget::Planner,
            Some("1.0"),
            send("hello", json!({})),
        )
        .await;
        let t = &v["result"]["task"];
        assert_eq!(t["status"]["state"], "TASK_STATE_COMPLETED", "{v}");
        assert_eq!(t["artifacts"][0]["name"], "response");
        assert_eq!(t["artifacts"][0]["parts"][0]["text"], "planner echo: hello");
        assert_eq!(t["history"][0]["role"], "ROLE_USER");
        assert_eq!(t["history"][0]["taskId"], t["id"]);
        // GetTask finds it; another endpoint does not.
        let id = t["id"].as_str().unwrap();
        let get = json!({"jsonrpc": "2.0", "id": 2, "method": "GetTask", "params": {"id": id, "historyLength": 0}});
        let g = call(&svc, &A2aTarget::Planner, Some("1.0"), get.clone()).await;
        assert_eq!(g["result"]["id"], id);
        assert!(g["result"].get("history").is_none());
        let other = call(&svc, &A2aTarget::Agent("x".into()), Some("1.0"), get).await;
        assert_eq!(other["error"]["code"], -32001);
    }

    #[tokio::test]
    async fn a_context_carries_earlier_turns() {
        let (svc, echo) = service();
        let first = call(
            &svc,
            &A2aTarget::Planner,
            None,
            send("one", json!({"contextId": "ctx-a"})),
        )
        .await;
        assert_eq!(first["result"]["task"]["contextId"], "ctx-a");
        call(
            &svc,
            &A2aTarget::Planner,
            None,
            send("two", json!({"contextId": "ctx-a"})),
        )
        .await;
        let turns = echo.turns.lock().unwrap();
        assert_eq!(
            turns[1].history,
            vec![("one".to_string(), "planner echo: one".to_string())]
        );
    }

    #[tokio::test]
    async fn a_failed_run_says_why() {
        let (svc, _) = service();
        let v = call(&svc, &A2aTarget::Planner, None, send("fail", json!({}))).await;
        let st = &v["result"]["task"]["status"];
        assert_eq!(st["state"], "TASK_STATE_FAILED");
        assert_eq!(st["message"]["parts"][0]["text"], "the agent broke");
    }

    #[tokio::test]
    async fn v03_clients_get_v03_answers() {
        let (svc, _) = service();
        let body = json!({"jsonrpc": "2.0", "id": "a", "method": "message/send", "params": {
            "message": {"kind": "message", "messageId": "m", "role": "user",
                        "parts": [{"kind": "text", "text": "old"}]}}});
        let v = call(&svc, &A2aTarget::Planner, None, body).await;
        let r = &v["result"];
        assert_eq!(r["kind"], "task", "{v}");
        assert_eq!(r["status"]["state"], "completed");
        assert_eq!(r["artifacts"][0]["parts"][0]["kind"], "text");
        let get =
            json!({"jsonrpc": "2.0", "id": "b", "method": "tasks/get", "params": {"id": r["id"]}});
        let g = call(&svc, &A2aTarget::Planner, Some("0.3"), get).await;
        assert_eq!(g["result"]["kind"], "task");
    }

    #[tokio::test]
    async fn non_blocking_then_cancel() {
        let (svc, _) = service();
        let mut body = send("slow", json!({}));
        body["params"]["configuration"] = json!({"returnImmediately": true});
        let v = call(&svc, &A2aTarget::Planner, Some("1.0"), body).await;
        let t = &v["result"]["task"];
        assert!(
            matches!(
                t["status"]["state"].as_str(),
                Some("TASK_STATE_SUBMITTED" | "TASK_STATE_WORKING")
            ),
            "{v}"
        );
        let id = t["id"].as_str().unwrap().to_string();
        let cancel =
            json!({"jsonrpc": "2.0", "id": 3, "method": "CancelTask", "params": {"id": id}});
        let c = call(&svc, &A2aTarget::Planner, Some("1.0"), cancel.clone()).await;
        assert_eq!(c["result"]["status"]["state"], "TASK_STATE_CANCELED", "{c}");
        let again = call(&svc, &A2aTarget::Planner, Some("1.0"), cancel).await;
        assert_eq!(again["error"]["code"], -32002);
        // A message to the canceled task is refused; its context is named.
        let follow = call(
            &svc,
            &A2aTarget::Planner,
            Some("1.0"),
            send("more", json!({"taskId": id})),
        )
        .await;
        assert_eq!(follow["error"]["code"], -32004);
        assert!(follow["error"]["message"]
            .as_str()
            .unwrap()
            .contains("send without taskId"));
    }

    #[tokio::test]
    async fn streaming_sends_task_artifact_then_status() {
        let (svc, _) = service();
        let mut body = send("streamed", json!({}));
        body["method"] = json!("SendStreamingMessage");
        let Reply::Stream {
            id,
            dialect,
            mut events,
        } = svc
            .handle(
                &A2aTarget::Planner,
                Some("1.0"),
                body.to_string().as_bytes(),
            )
            .await
        else {
            panic!("not a stream")
        };
        let mut seen = Vec::new();
        while let Some(e) = events.recv().await {
            seen.push(stream_event(&id, dialect, &e));
        }
        assert!(seen[0]["result"].get("task").is_some(), "{seen:?}");
        let art = seen
            .iter()
            .find(|e| e["result"].get("artifactUpdate").is_some())
            .unwrap();
        assert_eq!(
            art["result"]["artifactUpdate"]["artifact"]["parts"][0]["text"],
            "planner echo: streamed"
        );
        let last = seen.last().unwrap();
        assert_eq!(
            last["result"]["statusUpdate"]["status"]["state"],
            "TASK_STATE_COMPLETED"
        );
    }

    #[tokio::test]
    async fn refusals_carry_their_codes() {
        let (svc, _) = service();
        let t = A2aTarget::Planner;
        let v = call(&svc, &t, Some("1.0"), json!({"jsonrpc": "2.0", "id": 1, "method": "CreateTaskPushNotificationConfig", "params": {}})).await;
        assert_eq!(v["error"]["code"], -32003);
        let v = call(
            &svc,
            &t,
            Some("1.0"),
            json!({"jsonrpc": "2.0", "id": 1, "method": "GetExtendedAgentCard"}),
        )
        .await;
        assert_eq!(v["error"]["code"], -32004);
        let v = call(&svc, &t, Some("2.0"), send("x", json!({}))).await;
        assert_eq!(v["error"]["code"], -32009);
        let v = call(
            &svc,
            &t,
            Some("1.0"),
            send("x", json!({"parts": [{"url": "https://f/x.pdf"}]})),
        )
        .await;
        assert_eq!(v["error"]["code"], -32005);
        let v = call(&svc, &t, Some("1.0"), send("x", json!({"messageId": ""}))).await;
        assert_eq!(v["error"]["code"], -32602);
        let v = call(&svc, &t, Some("1.0"), send("x", json!({"taskId": "nope"}))).await;
        assert_eq!(v["error"]["code"], -32001);
        let mut out = send("x", json!({}));
        out["params"]["configuration"] = json!({"acceptedOutputModes": ["image/png"]});
        let v = call(&svc, &t, Some("1.0"), out).await;
        assert_eq!(v["error"]["code"], -32005);
        let v = call(
            &svc,
            &t,
            Some("0.3"),
            json!({"jsonrpc": "2.0", "id": 1, "method": "ListTasks", "params": {}}),
        )
        .await;
        assert_eq!(v["error"]["code"], -32601);
        match svc.handle(&t, None, b"{").await {
            Reply::Json(v) => assert_eq!(v["error"]["code"], -32700),
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn lists_this_endpoints_tasks() {
        let (svc, _) = service();
        call(&svc, &A2aTarget::Planner, None, send("a", json!({}))).await;
        call(
            &svc,
            &A2aTarget::Agent("x".into()),
            None,
            send("b", json!({})),
        )
        .await;
        let v = call(&svc, &A2aTarget::Planner, Some("1.0"), json!({"jsonrpc": "2.0", "id": 1, "method": "ListTasks", "params": {"includeArtifacts": true}})).await;
        assert_eq!(v["result"]["totalSize"], 1, "{v}");
        assert_eq!(v["result"]["nextPageToken"], "");
        assert_eq!(
            v["result"]["tasks"][0]["artifacts"][0]["parts"][0]["text"],
            "planner echo: a"
        );
    }

    #[tokio::test]
    async fn stop_all_fails_open_runs() {
        let (svc, _) = service();
        let mut body = send("slow", json!({}));
        body["params"]["configuration"] = json!({"returnImmediately": true});
        let v = call(&svc, &A2aTarget::Planner, None, body).await;
        let id = v["result"]["task"]["id"].as_str().unwrap().to_string();
        svc.stop_all();
        let g = call(
            &svc,
            &A2aTarget::Planner,
            None,
            json!({"jsonrpc": "2.0", "id": 1, "method": "GetTask", "params": {"id": id}}),
        )
        .await;
        assert_eq!(g["result"]["status"]["state"], "TASK_STATE_FAILED");
    }
}
