//! Per-turn chat orchestration service.
//!
//! `ChatRuntimeService` is the central application service for processing a
//! single user message through the full pipeline: flow resolution, memory
//! recall, prompt budget assembly, engine call (with multi-round tool
//! chaining), flow persistence, compaction, and token budget enforcement.
//!
//! Token budget gates:
//! - **80% warning**: returns a `system_notice` alerting the user.
//! - **100% hard limit**: blocks further requests until `/reset`.

use crate::application::engine_runtime::{
    collect_engine_response, ToolExecutor, ToolResultObserver,
};
use crate::application::flow_compaction::maybe_compact_flow;
use crate::application::memory_service::MemoryService;
use crate::application::ports::FlowStorePort;
use crate::application::prompt_budget::{assemble_recent_history, compute_base_input_budget};
use crate::domain::chat::{
    enforce_history_turn_limit, history_load_message_cap, resolve_flow_key, ChatLoopState,
    FlowCompactionPolicy,
};
use anyhow::Result;
use tengu_core::config::AgentConfig;
use tengu_core::token::estimate_tokens_approx_min1;
use tengu_core::types::{Message, Recipient, Role, ToolDef};
use tengu_core::{Engine, EngineContext, Refiner};

/// Result of processing a single user message through the chat pipeline.
///
/// `assistant_text` contains the model's response (if any).
/// `system_notice` contains budget warnings or limit-reached messages.
pub(crate) struct ChatTurnResult {
    pub assistant_text: Option<String>,
    pub system_notice: Option<String>,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
}

/// Central chat orchestration service.
///
/// Wired with references to the engine, refiner, flow store, memory service,
/// and tool executor. Call `process_user_text()` to run one full user→assistant
/// turn including memory recall, prompt budgeting, engine invocation, and
/// flow persistence.
pub(crate) struct ChatRuntimeService<'a> {
    pub engine: &'a dyn Engine,
    pub refiner: &'a dyn Refiner,
    pub flow_store: &'a dyn FlowStorePort,
    pub agent_id: &'a str,
    pub agent_config: &'a AgentConfig,
    pub history_turn_limit: usize,
    pub compaction_policy: FlowCompactionPolicy,
    pub system_prompt: String,
    pub tools: &'a [ToolDef],
    pub tool_executor: Option<&'a dyn ToolExecutor>,
    pub memory_service: Option<&'a MemoryService<'a>>,
    pub max_recall_entries: usize,
    pub max_recall_tokens: usize,
    pub tool_observer: Option<ToolResultObserver<'a>>,
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
}

impl<'a> ChatRuntimeService<'a> {
    pub(crate) async fn process_user_text(
        &self,
        state: &mut ChatLoopState,
        text: &str,
    ) -> Result<ChatTurnResult> {
        let original_len = text.len();
        let compressed = match self.refiner.compress(text).await {
            Ok(c) => c,
            Err(_) => text.to_string(),
        };
        if compressed.len() < original_len {
            state.tokens_saved += ((original_len - compressed.len()) / 4) as u32;
        }

        let sender = Recipient {
            pipe_id: "cli".to_string(),
            peer_id: "local".to_string(),
            account_id: None,
            thread_id: None,
        };
        let flow_key = resolve_flow_key(
            self.agent_id,
            &self.agent_config.flow.scope,
            &sender,
            state.manual_session_id.as_deref(),
        );
        let switched_flow = state.active_flow_key.as_deref() != Some(flow_key.as_str());
        if switched_flow {
            state.messages = self
                .flow_store
                .load_messages(&flow_key, history_load_message_cap(self.history_turn_limit))
                .unwrap_or_default();
            enforce_history_turn_limit(&mut state.messages, self.history_turn_limit);
            state.flow_token_usage = state
                .messages
                .iter()
                .map(|m| estimate_tokens_approx_min1(&m.content) as u64)
                .sum();
            state.active_flow_key = Some(flow_key.clone());
        }

        let user_message = Message {
            role: Role::User,
            content: compressed,
            tool_call_id: None,
            tool_calls: None,
        };
        let _ = self
            .flow_store
            .append_message(&flow_key, self.agent_id, &user_message);
        state.flow_token_usage += estimate_tokens_approx_min1(&user_message.content) as u64;
        state.messages.push(user_message);
        enforce_history_turn_limit(&mut state.messages, self.history_turn_limit);

        let _ = maybe_compact_flow(
            self.flow_store,
            &flow_key,
            self.agent_id,
            &mut state.messages,
            &mut state.flow_token_usage,
            self.refiner,
            self.compaction_policy,
            "pre-engine",
        )
        .await;

        if state.flow_token_usage >= self.agent_config.limits.max_tokens_per_flow {
            return Ok(ChatTurnResult {
                assistant_text: None,
                system_notice: Some(
                    "Flow token limit reached. Use /reset to start a new session.".into(),
                ),
                total_input_tokens: state.total_input_tokens,
                total_output_tokens: state.total_output_tokens,
            });
        }

        // Recall relevant memories if memory service is available.
        let memory_block = if let Some(mem) = self.memory_service {
            match mem
                .recall(
                    &state
                        .messages
                        .last()
                        .map(|m| m.content.as_str())
                        .unwrap_or(""),
                    self.max_recall_entries,
                    self.max_recall_tokens,
                )
                .await
            {
                Ok(results) if !results.is_empty() => {
                    let mut block = String::from("[Relevant memories]\n");
                    for r in &results {
                        block.push_str(&format!("- {}\n", r.entry.content));
                    }
                    Some(block)
                }
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(error = %e, "Memory recall failed, continuing without memories");
                    None
                }
            }
        } else {
            None
        };

        let memory_tokens = memory_block
            .as_ref()
            .map(|b| estimate_tokens_approx_min1(b))
            .unwrap_or(0);

        let remaining_flow_tokens = self
            .agent_config
            .limits
            .max_tokens_per_flow
            .saturating_sub(state.flow_token_usage);
        let engine_output_cap = self.engine.max_output_tokens_per_turn() as usize;
        let base_input_budget = compute_base_input_budget(
            self.engine.context_window(),
            engine_output_cap,
            &self.system_prompt,
            remaining_flow_tokens,
        );
        let history_budget = base_input_budget.saturating_sub(memory_tokens);
        let history_assembly = assemble_recent_history(&state.messages, history_budget);

        let mut prompt_messages = Vec::new();
        if let Some(ref block) = memory_block {
            prompt_messages.push(Message {
                role: Role::System,
                content: block.clone(),
                tool_call_id: None,
                tool_calls: None,
            });
        }
        prompt_messages.extend(history_assembly.messages.clone());
        if prompt_messages.is_empty() {
            return Ok(ChatTurnResult {
                assistant_text: None,
                system_notice: Some("Context budget exhausted. Use /reset to continue.".into()),
                total_input_tokens: state.total_input_tokens,
                total_output_tokens: state.total_output_tokens,
            });
        }

        let context = EngineContext {
            workspace: self.agent_config.workspace.clone(),
            system_prompt: Some(self.system_prompt.clone()),
        };
        let resp = collect_engine_response(
            self.engine,
            &prompt_messages,
            self.tools,
            &context,
            self.tool_executor,
            self.tool_observer,
            self.cancel,
        )
        .await?;

        state.total_input_tokens += resp.input_tokens_delta;
        state.total_output_tokens += resp.output_tokens_delta;
        let response_text = resp.text;

        if !response_text.is_empty() {
            let assistant_message = Message {
                role: Role::Assistant,
                content: response_text.clone(),
                tool_call_id: None,
                tool_calls: None,
            };
            let _ = self
                .flow_store
                .append_message(&flow_key, self.agent_id, &assistant_message);
            state.flow_token_usage +=
                estimate_tokens_approx_min1(&assistant_message.content) as u64;
            state.messages.push(assistant_message);
            enforce_history_turn_limit(&mut state.messages, self.history_turn_limit);

            let _ = maybe_compact_flow(
                self.flow_store,
                &flow_key,
                self.agent_id,
                &mut state.messages,
                &mut state.flow_token_usage,
                self.refiner,
                self.compaction_policy,
                "post-engine",
            )
            .await;
        }

        // Warn when approaching the token limit (80% threshold).
        let max_tokens = self.agent_config.limits.max_tokens_per_flow;
        let usage_pct = if max_tokens > 0 {
            (state.flow_token_usage as f64 / max_tokens as f64 * 100.0) as u32
        } else {
            0
        };
        let budget_notice = if usage_pct >= 80 {
            let remaining = max_tokens.saturating_sub(state.flow_token_usage);
            Some(format!(
                "⚠️ Token budget: {}% used ({}/{} tokens, ~{} remaining). Use /reset to start fresh.",
                usage_pct, state.flow_token_usage, max_tokens, remaining
            ))
        } else {
            None
        };

        Ok(ChatTurnResult {
            assistant_text: if response_text.is_empty() {
                None
            } else {
                Some(response_text)
            },
            system_notice: budget_notice,
            total_input_tokens: state.total_input_tokens,
            total_output_tokens: state.total_output_tokens,
        })
    }
}
