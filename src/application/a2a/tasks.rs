//! The task store of `tengu a2a serve` (in memory, one per process) and the
//! pure helpers the service uses: a message's parts → the turn text, the
//! `historyLength` rule, `ListTasks` filters and pages.
//!
//! | Rule | Value |
//! |---|---|
//! | Ownership | a task belongs to the endpoint (`A2aTarget`) that made it; another endpoint gets `TaskNotFound` (spec § 13.1: never reveal it) |
//! | Capacity | `max_tasks`; a new task evicts the settled task updated longest ago; all unsettled ⇒ refused (busy) |
//! | Context memory | per (endpoint, `contextId`): the last `context_turns` completed `(client text, answer)` pairs, handed to the next turn |
//! | Paging | `pageToken` = `o<offset>` into the filtered list, newest status first; `nextPageToken` `""` on the last page |

use std::collections::{HashMap, VecDeque};

use base64::Engine as _;
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::AbortHandle;

use crate::domain::a2a::model::{
    ListTasksRequest, ListTasksResponse, Part, PartKind, Task, TaskState,
};
use crate::domain::a2a::rpc::RpcError;
use crate::domain::a2a::{iso_ms, parse_iso_ms};
use crate::ports::a2a::A2aTarget;

/// One stored task.
pub(crate) struct Entry {
    pub target: A2aTarget,
    pub task: Task,
    pub updated_ms: i64,
    pub tx: watch::Sender<Task>,
    pub abort: Option<AbortHandle>,
}

/// Every task and context of one server.
#[derive(Default)]
pub(crate) struct Store {
    pub tasks: HashMap<String, Entry>,
    pub contexts: HashMap<(A2aTarget, String), VecDeque<(String, String)>>,
}

impl Store {
    /// The task `id` of `target`.
    pub fn get(&self, target: &A2aTarget, id: &str) -> Result<&Entry, RpcError> {
        self.tasks
            .get(id)
            .filter(|e| &e.target == target)
            .ok_or_else(|| RpcError::task_not_found(id))
    }

    /// Make room for one more task (module table: capacity).
    pub fn make_room(&mut self, max_tasks: usize) -> Result<(), RpcError> {
        while self.tasks.len() >= max_tasks {
            let oldest = self
                .tasks
                .iter()
                .filter(|(_, e)| e.task.status.state.is_settled())
                .min_by_key(|(id, e)| (e.updated_ms, (*id).clone()))
                .map(|(id, _)| id.clone());
            match oldest {
                Some(id) => {
                    self.tasks.remove(&id);
                }
                None => {
                    return Err(RpcError::internal(format!(
                        "busy: {max_tasks} unfinished tasks — retry later"
                    )))
                }
            }
        }
        Ok(())
    }

    /// Earlier turns of a context, oldest first.
    pub fn history(&self, target: &A2aTarget, context_id: &str) -> Vec<(String, String)> {
        self.contexts
            .get(&(target.clone(), context_id.to_string()))
            .map(|d| d.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Remember a completed turn (at most `keep` per context).
    pub fn remember(
        &mut self,
        target: &A2aTarget,
        context_id: &str,
        turn: (String, String),
        keep: usize,
    ) {
        if keep == 0 {
            return;
        }
        let d = self
            .contexts
            .entry((target.clone(), context_id.to_string()))
            .or_default();
        d.push_back(turn);
        while d.len() > keep {
            d.pop_front();
        }
    }

    /// Change task `id`'s status (and artifacts) unless it is terminal
    /// already (a cancel won), stamp it `now_ms`, publish it. `false` = it
    /// was terminal, nothing changed.
    pub fn update(&mut self, id: &str, now_ms: i64, f: impl FnOnce(&mut Task)) -> bool {
        let Some(e) = self.tasks.get_mut(id) else {
            return false;
        };
        if e.task.status.state.is_terminal() {
            return false;
        }
        f(&mut e.task);
        e.task.status.timestamp = Some(iso_ms(now_ms));
        e.updated_ms = now_ms;
        e.tx.send_replace(e.task.clone());
        true
    }

    /// `ListTasks` over `target`'s tasks (module table: paging).
    pub fn list(
        &self,
        target: &A2aTarget,
        req: &ListTasksRequest,
    ) -> Result<ListTasksResponse, RpcError> {
        let page_size = req.page_size.unwrap_or(50);
        if !(1..=100).contains(&page_size) {
            return Err(RpcError::invalid_params(
                "pageSize",
                format!("must be between 1 and 100 inclusive, got {page_size}"),
            ));
        }
        check_history_length(req.history_length)?;
        let status = match req.status.as_deref().filter(|s| !s.is_empty()) {
            None => None,
            Some(s) => match TaskState::parse(s).filter(|t| *t != TaskState::Unspecified) {
                Some(t) => Some(t),
                None => {
                    let valid: Vec<&str> = TaskState::ALL.iter().map(|t| t.proto_name()).collect();
                    return Err(RpcError::invalid_params(
                        "status",
                        format!(
                            "invalid status value '{s}'; must be one of: {}",
                            valid.join(", ")
                        ),
                    ));
                }
            },
        };
        let after = match req.status_timestamp_after.as_deref() {
            None => None,
            Some(s) => Some(parse_iso_ms(s).ok_or_else(|| {
                RpcError::invalid_params(
                    "statusTimestampAfter",
                    format!("not an ISO 8601 time: '{s}'"),
                )
            })?),
        };
        let offset = match req.page_token.as_deref().filter(|t| !t.is_empty()) {
            None => 0,
            Some(t) => t
                .strip_prefix('o')
                .and_then(|n| n.parse::<usize>().ok())
                .ok_or_else(|| {
                    RpcError::invalid_params("pageToken", "not a token this server issued")
                })?,
        };
        let mut rows: Vec<&Entry> = self
            .tasks
            .values()
            .filter(|e| &e.target == target)
            .filter(|e| {
                req.context_id
                    .as_ref()
                    .map_or(true, |c| &e.task.context_id == c)
            })
            .filter(|e| status.map_or(true, |s| e.task.status.state == s))
            .filter(|e| after.map_or(true, |a| e.updated_ms >= a))
            .collect();
        rows.sort_by(|a, b| {
            b.updated_ms
                .cmp(&a.updated_ms)
                .then(a.task.id.cmp(&b.task.id))
        });
        let total = rows.len();
        let size = page_size as usize;
        let tasks = rows
            .iter()
            .skip(offset)
            .take(size)
            .map(|e| {
                let mut t = with_history(e.task.clone(), req.history_length);
                if !req.include_artifacts.unwrap_or(false) {
                    t.artifacts = None;
                }
                t
            })
            .collect();
        Ok(ListTasksResponse {
            tasks,
            next_page_token: if offset + size < total {
                format!("o{}", offset + size)
            } else {
                String::new()
            },
            page_size,
            total_size: total as i64,
        })
    }
}

/// `historyLength` must be ≥ 0.
pub fn check_history_length(n: Option<i64>) -> Result<(), RpcError> {
    match n {
        Some(n) if n < 0 => Err(RpcError::invalid_params(
            "historyLength",
            format!("must be a non-negative integer, got {n}"),
        )),
        _ => Ok(()),
    }
}

/// `t` with at most `n` recent history messages (unset = all, 0 = none).
pub fn with_history(mut t: Task, n: Option<i64>) -> Task {
    match n {
        None => {}
        Some(0) => t.history = None,
        Some(n) => {
            if let Some(h) = t.history.as_mut() {
                let drop = h.len().saturating_sub(n as usize);
                h.drain(..drop);
            }
        }
    }
    t
}

/// Media types a `raw` part may carry (decoded as UTF-8 text).
fn texty(media_type: Option<&str>) -> bool {
    media_type.is_some_and(|m| {
        let m = m
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        m.starts_with("text/") || m == "application/json" || m.ends_with("+json")
    })
}

/// A message's parts as one turn text: text as is, data as fenced JSON, a
/// text-like `raw` decoded; a `url` (tengu fetches nothing) or a binary
/// `raw` is `ContentTypeNotSupported`.
pub fn turn_text(parts: &[Part]) -> Result<String, RpcError> {
    if parts.is_empty() {
        return Err(RpcError::invalid_params(
            "message.parts",
            "at least one part is required",
        ));
    }
    let mut out: Vec<String> = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        match p.kind() {
            None => {
                return Err(RpcError::invalid_params(
                    &format!("message.parts[{i}]"),
                    "a part holds exactly one of text, raw, url, data",
                ))
            }
            Some(PartKind::Text) => out.push(p.text.clone().unwrap_or_default()),
            Some(PartKind::Data) => out.push(format!(
                "```json\n{}\n```",
                serde_json::to_string_pretty(p.data.as_ref().unwrap_or(&Value::Null))
                    .unwrap_or_default()
            )),
            Some(PartKind::Raw) if texty(p.media_type.as_deref()) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(p.raw.as_deref().unwrap_or("").trim())
                    .map_err(|e| {
                        RpcError::invalid_params(&format!("message.parts[{i}].raw"), format!("not base64: {e}"))
                    })?;
                let text = String::from_utf8(bytes).map_err(|_| {
                    RpcError::content_type(format!("message.parts[{i}]: the bytes are not UTF-8 text"))
                })?;
                match &p.filename {
                    Some(f) => out.push(format!("[file {f}]\n{text}")),
                    None => out.push(text),
                }
            }
            Some(PartKind::Raw) => {
                return Err(RpcError::content_type(format!(
                    "message.parts[{i}]: media type {} — this agent takes text/plain, application/json and text files",
                    p.media_type.as_deref().unwrap_or("(none)")
                )))
            }
            Some(PartKind::Url) => {
                return Err(RpcError::content_type(format!(
                    "message.parts[{i}]: file URLs are not fetched — send the content as text or data"
                )))
            }
        }
    }
    let text = out.join("\n\n");
    if text.trim().is_empty() {
        return Err(RpcError::invalid_params(
            "message.parts",
            "the message has no text",
        ));
    }
    Ok(text)
}

/// The client can read tengu's `text/plain` answer: `acceptedOutputModes`
/// empty, or naming any text type, `*/*`, or JSON (an answer is often JSON
/// text) — a client that takes only images, audio … is refused.
pub fn accepts_text(modes: &[String]) -> bool {
    modes.is_empty()
        || modes.iter().any(|m| {
            let m = m
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            m.starts_with("text/") || m == "*/*" || m == "application/json" || m.ends_with("+json")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::a2a::model::{Message, TaskStatus};
    use serde_json::json;

    fn entry(target: A2aTarget, id: &str, ctx: &str, state: TaskState, at: i64) -> (String, Entry) {
        let task = Task {
            id: id.into(),
            context_id: ctx.into(),
            status: TaskStatus {
                state,
                message: None,
                timestamp: Some(iso_ms(at)),
            },
            artifacts: Some(vec![]),
            history: Some(vec![Message::default(), Message::default()]),
            metadata: None,
        };
        let (tx, _) = watch::channel(task.clone());
        (
            id.to_string(),
            Entry {
                target,
                task,
                updated_ms: at,
                tx,
                abort: None,
            },
        )
    }

    fn store() -> Store {
        let mut s = Store::default();
        for (id, ctx, st, at) in [
            ("t1", "c1", TaskState::Completed, 1_000),
            ("t2", "c1", TaskState::Working, 3_000),
            ("t3", "c2", TaskState::Completed, 2_000),
        ] {
            let (k, e) = entry(A2aTarget::Planner, id, ctx, st, at);
            s.tasks.insert(k, e);
        }
        let (k, e) = entry(
            A2aTarget::Agent("x".into()),
            "t4",
            "c1",
            TaskState::Completed,
            9_000,
        );
        s.tasks.insert(k, e);
        s
    }

    #[test]
    fn tasks_belong_to_their_endpoint() {
        let s = store();
        assert!(s.get(&A2aTarget::Planner, "t1").is_ok());
        assert_eq!(s.get(&A2aTarget::Planner, "t4").err().unwrap().code, -32001);
    }

    #[test]
    fn lists_newest_first_in_pages() {
        let s = store();
        let req = ListTasksRequest {
            page_size: Some(2),
            ..Default::default()
        };
        let p1 = s.list(&A2aTarget::Planner, &req).unwrap();
        let ids: Vec<&str> = p1.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["t2", "t3"]);
        assert_eq!((p1.total_size, p1.next_page_token.as_str()), (3, "o2"));
        assert!(p1.tasks[0].artifacts.is_none());
        let p2 = s
            .list(
                &A2aTarget::Planner,
                &ListTasksRequest {
                    page_size: Some(2),
                    page_token: Some("o2".into()),
                    include_artifacts: Some(true),
                    history_length: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(p2.tasks[0].id, "t1");
        assert_eq!(p2.next_page_token, "");
        assert_eq!(p2.tasks[0].artifacts, Some(vec![]));
        assert_eq!(p2.tasks[0].history.as_ref().unwrap().len(), 1);
        let working = s
            .list(
                &A2aTarget::Planner,
                &ListTasksRequest {
                    status: Some("TASK_STATE_WORKING".into()),
                    context_id: Some("c1".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(working.tasks.len(), 1);
    }

    #[test]
    fn list_validates_its_params() {
        let s = store();
        for (req, field) in [
            (
                ListTasksRequest {
                    page_size: Some(150),
                    ..Default::default()
                },
                "pageSize",
            ),
            (
                ListTasksRequest {
                    history_length: Some(-5),
                    ..Default::default()
                },
                "historyLength",
            ),
            (
                ListTasksRequest {
                    status: Some("TASK_STATE_RUNNING".into()),
                    ..Default::default()
                },
                "status",
            ),
            (
                ListTasksRequest {
                    page_token: Some("zz".into()),
                    ..Default::default()
                },
                "pageToken",
            ),
            (
                ListTasksRequest {
                    status_timestamp_after: Some("x".into()),
                    ..Default::default()
                },
                "statusTimestampAfter",
            ),
        ] {
            let e = s.list(&A2aTarget::Planner, &req).unwrap_err();
            assert_eq!(e.code, -32602);
            assert!(e.message.contains(field), "{e}");
        }
    }

    #[test]
    fn room_is_made_from_the_oldest_settled_task() {
        let mut s = store();
        s.make_room(4).unwrap();
        assert!(!s.tasks.contains_key("t1"), "t1 is the oldest settled");
        assert!(s.tasks.contains_key("t2"), "never an unsettled one");
        let mut busy = Store::default();
        let (k, e) = entry(A2aTarget::Planner, "w", "c", TaskState::Working, 1);
        busy.tasks.insert(k, e);
        assert!(busy.make_room(1).unwrap_err().message.contains("busy"));
    }

    #[test]
    fn a_terminal_task_never_changes() {
        let mut s = store();
        assert!(!s.update("t1", 5_000, |t| t.status.state = TaskState::Failed));
        assert!(s.update("t2", 5_000, |t| t.status.state = TaskState::Completed));
        let t2 = &s.tasks["t2"];
        assert_eq!(
            t2.task.status.timestamp.as_deref(),
            Some("1970-01-01T00:00:05.000Z")
        );
        assert_eq!(t2.tx.borrow().status.state, TaskState::Completed);
    }

    #[test]
    fn contexts_keep_the_last_turns() {
        let mut s = Store::default();
        for i in 0..4 {
            s.remember(
                &A2aTarget::Planner,
                "c",
                (format!("q{i}"), format!("a{i}")),
                2,
            );
        }
        assert_eq!(
            s.history(&A2aTarget::Planner, "c"),
            vec![
                ("q2".to_string(), "a2".to_string()),
                ("q3".to_string(), "a3".to_string())
            ]
        );
        assert!(s.history(&A2aTarget::Agent("x".into()), "c").is_empty());
    }

    #[test]
    fn parts_become_the_turn_text() {
        let raw = base64::engine::general_purpose::STANDARD.encode("file body");
        let text = turn_text(&[
            Part::text("hello"),
            Part::data(json!({"a": 1})),
            Part {
                raw: Some(raw),
                media_type: Some("text/csv".into()),
                filename: Some("x.csv".into()),
                ..Default::default()
            },
        ])
        .unwrap();
        assert_eq!(
            text,
            "hello\n\n```json\n{\n  \"a\": 1\n}\n```\n\n[file x.csv]\nfile body"
        );
        let e = turn_text(&[Part {
            url: Some("https://x/a.pdf".into()),
            ..Default::default()
        }])
        .unwrap_err();
        assert_eq!(e.code, -32005);
        let e = turn_text(&[Part {
            raw: Some("AAAA".into()),
            media_type: Some("image/png".into()),
            ..Default::default()
        }])
        .unwrap_err();
        assert_eq!(e.code, -32005);
        assert_eq!(turn_text(&[]).unwrap_err().code, -32602);
        assert_eq!(turn_text(&[Part::default()]).unwrap_err().code, -32602);
    }

    #[test]
    fn history_length_trims() {
        let (_, e) = entry(A2aTarget::Planner, "t", "c", TaskState::Completed, 1);
        assert_eq!(with_history(e.task.clone(), None).history.unwrap().len(), 2);
        assert_eq!(
            with_history(e.task.clone(), Some(1)).history.unwrap().len(),
            1
        );
        assert!(with_history(e.task, Some(0)).history.is_none());
        assert!(check_history_length(Some(-1)).is_err());
    }

    #[test]
    fn output_modes() {
        assert!(accepts_text(&[]));
        assert!(accepts_text(&[
            "application/json".into(),
            "text/plain".into()
        ]));
        assert!(!accepts_text(&["image/png".into()]));
    }
}
