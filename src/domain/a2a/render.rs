//! Text a model (the `a2a` tool) or the operator (`tengu a2a`) reads for a
//! card, a task or a message. Ids are always whole (task, context, message,
//! artifact); only content text is cut, at `max_chars`, with the count of
//! what was cut.
//!
//! | Object | Line 1 | Then |
//! |---|---|---|
//! | task | `a2a <remote>: task <id> <STATE> context <id>` | the status message, each artifact (`artifact <name> <id>:` + its parts' text), a `next:` hint for a task that is not done |
//! | message | `a2a <remote>: message <id> context <id>` | its parts' text |
//! | card | `a2a <remote>: <name> v<version>` | description, interface used, capabilities, auth schemes, one line per skill |

use super::model::{parts_text, AgentCard, Message, SendResult, Task, TaskState};

/// `text` cut to `max_chars` characters (`0` = no cut), noting the rest.
pub fn cap(text: &str, max_chars: usize) -> String {
    let n = text.chars().count();
    if max_chars == 0 || n <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}\n… [{} more chars cut]", n - max_chars)
}

/// A send result.
pub fn send_result(remote: &str, r: &SendResult, max_chars: usize) -> String {
    match r {
        SendResult::Task(t) => task(remote, t, max_chars),
        SendResult::Message(m) => message(remote, m, max_chars),
    }
}

/// A direct reply message.
pub fn message(remote: &str, m: &Message, max_chars: usize) -> String {
    let mut out = format!(
        "a2a {remote}: message {} context {}",
        m.message_id,
        m.context_id.as_deref().unwrap_or("-")
    );
    let body = parts_text(&m.parts);
    if !body.is_empty() {
        out.push('\n');
        out.push_str(&cap(&body, max_chars));
    }
    out
}

/// A task and what to do next.
pub fn task(remote: &str, t: &Task, max_chars: usize) -> String {
    let mut out = format!(
        "a2a {remote}: task {} {} context {}",
        t.id,
        t.status.state.proto_name(),
        if t.context_id.is_empty() {
            "-"
        } else {
            &t.context_id
        }
    );
    // One budget for the status message and every artifact.
    let mut left = max_chars;
    let mut take = |text: &str| -> String {
        if max_chars == 0 {
            return text.to_string();
        }
        let n = text.chars().count();
        if left == 0 {
            return format!("… [{n} chars cut]");
        }
        let s = cap(text, left);
        left -= n.min(left);
        s
    };
    if let Some(m) = &t.status.message {
        let body = parts_text(&m.parts);
        if !body.is_empty() {
            out.push_str("\nstatus: ");
            out.push_str(&take(&body));
        }
    }
    for a in t.artifacts.iter().flatten() {
        out.push_str(&format!(
            "\nartifact {} {}:\n",
            a.name.as_deref().unwrap_or("-"),
            a.artifact_id
        ));
        out.push_str(&take(&parts_text(&a.parts)));
    }
    let next = match t.status.state {
        TaskState::InputRequired => Some(format!(
            "the agent needs more input — send again with task_id {} and context_id {}",
            t.id, t.context_id
        )),
        TaskState::AuthRequired => {
            Some("the agent needs authorization tengu cannot give — tell the operator".to_string())
        }
        TaskState::Submitted | TaskState::Working | TaskState::Unspecified => Some(format!(
            "not done yet — call get with task_id {} later (or cancel it)",
            t.id
        )),
        _ => None,
    };
    if let Some(n) = next {
        out.push_str("\nnext: ");
        out.push_str(&n);
    }
    out
}

/// A card: who the agent is and what it does.
pub fn card(remote: &str, c: &AgentCard) -> String {
    let mut out = format!("a2a {remote}: {} v{}\n{}", c.name, c.version, c.description);
    match c.endpoint() {
        Ok(e) => out.push_str(&format!(
            "\ninterface: {:?} {} at {}{}",
            e.binding,
            e.dialect.version(),
            e.url,
            e.tenant.map(|t| format!(" tenant {t}")).unwrap_or_default()
        )),
        Err(why) => out.push_str(&format!("\ninterface: none usable — {why}")),
    }
    let caps = &c.capabilities;
    out.push_str(&format!(
        "\ncapabilities: streaming={} push={}",
        caps.streaming.unwrap_or(false),
        caps.push_notifications.unwrap_or(false)
    ));
    let schemes = c.required_schemes();
    if !schemes.is_empty() {
        out.push_str(&format!("\nauth: {}", schemes.join(", ")));
    }
    out.push_str(&format!("\nskills ({}):", c.skills.len()));
    for s in &c.skills {
        out.push_str(&format!("\n- {} ({}): {}", s.id, s.name, s.description));
        if !s.examples.is_empty() {
            out.push_str(&format!(" e.g. {}", s.examples.join(" | ")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::a2a::model::{Artifact, Part, TaskStatus};

    #[test]
    fn a_task_shows_ids_whole_and_cuts_content_only() {
        let id = "0f7c2a8e-5b1d-4c3e-9a7f-1234567890ab";
        let t = Task {
            id: id.into(),
            context_id: "ctx-0f7c2a8e-5b1d-4c3e-9a7f".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                ..Default::default()
            },
            artifacts: Some(vec![Artifact {
                artifact_id: "art-1".into(),
                name: Some("response".into()),
                parts: vec![Part::text("x".repeat(50))],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let text = task("research", &t, 10);
        assert!(text.starts_with(&format!(
            "a2a research: task {id} TASK_STATE_COMPLETED context ctx-0f7c2a8e-5b1d-4c3e-9a7f"
        )));
        assert!(text.contains("artifact response art-1:\nxxxxxxxxxx\n… [40 more chars cut]"));
        assert!(!text.contains("next:"));
    }

    #[test]
    fn unsettled_tasks_say_what_next() {
        let mut t = Task {
            id: "t1".into(),
            context_id: "c1".into(),
            ..Default::default()
        };
        t.status.state = TaskState::InputRequired;
        assert!(task("r", &t, 0).contains("send again with task_id t1 and context_id c1"));
        t.status.state = TaskState::Working;
        assert!(task("r", &t, 0).contains("call get with task_id t1"));
    }

    #[test]
    fn cap_counts_characters() {
        assert_eq!(cap("héllo", 0), "héllo");
        assert_eq!(cap("héllo", 2), "hé\n… [3 more chars cut]");
    }
}
