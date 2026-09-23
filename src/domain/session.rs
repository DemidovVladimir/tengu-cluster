//! Chat/flow session state — per-session prompt assembly and loop state.
//! Pure data, no IO.

use crate::domain::message::Lens;
use crate::domain::message::Message;

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
pub(crate) struct HistoryAssembly {
    pub messages: Vec<Message>,
    #[allow(dead_code)] // read in tests only
    pub used_tokens: usize,
    #[allow(dead_code)] // read in tests only
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
    }
}
