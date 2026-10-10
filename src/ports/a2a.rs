//! A2A server port: who answers a message another harness sends to tengu
//! (`application/a2a/`). Impl: `bootstrap/a2a.rs` (`A2aTurnRunner` — the
//! planner front door or one exposed agent, in-process).

use async_trait::async_trait;

/// The tengu side of a served endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum A2aTarget {
    /// The planner (`[orchestrator]`): `/a2a`.
    Planner,
    /// One agent (`[agents.<name>]` with a `description`):
    /// `/a2a/agents/<name>`.
    Agent(String),
}

impl A2aTarget {
    /// `planner` or `agent:<name>` (logs, errors).
    pub(crate) fn label(&self) -> String {
        match self {
            A2aTarget::Planner => "planner".into(),
            A2aTarget::Agent(a) => format!("agent:{a}"),
        }
    }
}

/// One inbound turn.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct A2aTurn {
    pub task_id: String,
    pub context_id: String,
    /// The message's text and data parts, as one text.
    pub text: String,
    /// Earlier turns of the context, oldest first: `(client text, answer)`.
    pub history: Vec<(String, String)>,
}

/// Runs a turn and returns the final answer. The text — and an error's
/// message — leave tengu as they are: an impl redacts registered secrets.
#[async_trait]
pub(crate) trait A2aRunner: Send + Sync {
    async fn run(&self, target: &A2aTarget, turn: A2aTurn) -> anyhow::Result<String>;
}
