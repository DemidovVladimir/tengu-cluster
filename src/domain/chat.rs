use tengu_core::types::{Message, Recipient, Role};
use tengu_core::Lens;

#[derive(Debug, Clone, Default)]
pub(crate) struct PromptAssemblyReport {
    pub system_tokens: usize,
    pub history_tokens: usize,
    pub dropped_history_messages: usize,
    pub reserved_output_tokens: usize,
    pub output_token_cap: usize,
    pub total_input_budget: usize,
    pub flow_budget_remaining: usize,
    pub compaction_applied: bool,
    pub compacted_messages: usize,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub(crate) struct HistoryAssembly {
    pub messages: Vec<Message>,
    pub used_tokens: usize,
    pub dropped_messages: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FlowCompactionPolicy {
    pub threshold_tokens: u64,
    pub keep_turns: usize,
    pub summary_max_tokens: u32,
}

/// Mutable per-session runtime state for chat loop execution.
#[derive(Debug, Clone)]
pub(crate) struct ChatLoopState {
    pub messages: Vec<Message>,
    pub active_flow_key: Option<String>,
    pub manual_session_id: Option<String>,
    pub flow_token_usage: u64,
    pub active_lens: Lens,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub tokens_saved: u32,
    pub last_prompt_report: Option<PromptAssemblyReport>,
}

impl ChatLoopState {
    pub(crate) fn reset_for_new_session(&mut self) {
        self.manual_session_id = Some(uuid::Uuid::new_v4().to_string());
        self.active_flow_key = None;
        self.messages.clear();
        self.flow_token_usage = 0;
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
        self.tokens_saved = 0;
    }
}

pub(crate) fn resolve_history_turn_limit(flow: &tengu_core::config::FlowConfig) -> usize {
    flow.max_history_turns
        .map(|v| v.max(1) as usize)
        .unwrap_or_else(|| default_history_turn_limit_for_scope(&flow.scope))
}

pub(crate) fn default_history_turn_limit_for_scope(scope: &str) -> usize {
    match scope {
        "main" => 40,
        "per-group" => 30,
        "per-pipe-sender" => 25,
        _ => 20,
    }
}

pub(crate) fn history_load_message_cap(history_turn_limit: usize) -> usize {
    history_turn_limit.saturating_mul(4).max(64)
}

pub(crate) fn enforce_history_turn_limit(messages: &mut Vec<Message>, max_turns: usize) -> usize {
    if max_turns == 0 || messages.is_empty() {
        let dropped = messages.len();
        messages.clear();
        return dropped;
    }

    let mut seen_user_turns = 0usize;
    let mut start_index = None;

    for (idx, message) in messages.iter().enumerate().rev() {
        if matches!(message.role, Role::User) {
            seen_user_turns += 1;
            if seen_user_turns == max_turns {
                start_index = Some(idx);
                break;
            }
        }
    }

    let Some(start) = start_index else {
        return 0;
    };

    if start == 0 {
        return 0;
    }

    messages.drain(0..start);
    start
}

/// Resolve a deterministic flow key from scope and sender identity.
pub(crate) fn resolve_flow_key(
    agent_id: &str,
    scope: &str,
    sender: &Recipient,
    manual_session_id: Option<&str>,
) -> String {
    let base = match scope {
        "main" => format!("{}:main", agent_id),
        "per-pipe-sender" => format!("{}:{}:{}", agent_id, sender.pipe_id, sender.peer_id),
        "per-group" => format!(
            "{}:{}:{}",
            agent_id,
            sender.pipe_id,
            sender
                .thread_id
                .as_deref()
                .or(sender.account_id.as_deref())
                .unwrap_or(sender.peer_id.as_str())
        ),
        _ => format!("{}:{}", agent_id, sender.peer_id), // per-sender default
    };

    match manual_session_id {
        Some(session) if !session.is_empty() => format!("{}:{}", base, session),
        _ => base,
    }
}
