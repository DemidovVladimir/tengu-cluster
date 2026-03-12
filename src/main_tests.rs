//! Tests for binary runtime.

#[cfg(test)]
mod tests {
    use crate::application::prompt_budget::{
        assemble_recent_history, compute_base_input_budget, compute_total_input_budget,
        reserved_output_tokens,
    };
    use crate::*;

    fn msg(content: &str) -> Message {
        Message {
            role: Role::User,
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn assistant_msg(content: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    #[test]
    fn resolve_models_probe_url_normalizes_v1_suffix() {
        assert_eq!(
            resolve_models_probe_url("https://api.openai.com"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            resolve_models_probe_url("https://router.huggingface.co/v1"),
            "https://router.huggingface.co/v1/models"
        );
        assert_eq!(
            resolve_models_probe_url("https://api.anthropic.com/v1/"),
            "https://api.anthropic.com/v1/models"
        );
    }

    #[test]
    fn first_output_line_prefers_stderr_then_stdout() {
        let stderr = b"\nwarn line\n";
        let stdout = b"\nstdout line\n";
        assert_eq!(
            first_output_line(stdout, stderr).as_deref(),
            Some("warn line")
        );
        assert_eq!(
            first_output_line(b"\n", b"\nstdout only\n").as_deref(),
            Some("stdout only")
        );
        assert_eq!(first_output_line(b"\n", b"\n"), None);
    }

    #[test]
    fn history_drop_policy_keeps_newest_contiguous_suffix() {
        let messages = vec![
            msg(&"a".repeat(40)),
            msg(&"b".repeat(400)),
            msg(&"c".repeat(40)),
        ];
        let assembled = assemble_recent_history(&messages, 25);

        assert_eq!(assembled.messages.len(), 1);
        assert_eq!(assembled.messages[0].content, "c".repeat(40));
        assert_eq!(assembled.dropped_messages, 2);
    }

    #[test]
    fn budget_overflow_small_context_window_preserves_output_reserve() {
        let total_input_budget = compute_total_input_budget(128, 1_024, 10_000);
        assert_eq!(total_input_budget, 0);
    }

    #[test]
    fn usage_accounting_keeps_latest_turn_snapshot() {
        let mut snapshot = None;
        absorb_turn_usage_snapshot(&mut snapshot, 120, 20);
        absorb_turn_usage_snapshot(&mut snapshot, 130, 30);

        assert_eq!(snapshot, Some((130, 30)));
    }

    #[test]
    fn usage_accounting_applies_turn_snapshot_once_to_session_totals() {
        let mut total_in = 100u32;
        let mut total_out = 40u32;
        apply_turn_usage_to_session_totals(&mut total_in, &mut total_out, Some((50, 10)));
        apply_turn_usage_to_session_totals(&mut total_in, &mut total_out, None);

        assert_eq!(total_in, 150);
        assert_eq!(total_out, 50);
    }

    #[test]
    fn budget_overflow_remaining_flow_tokens_hard_caps_input_budget() {
        let total_input_budget = compute_total_input_budget(8_192, 1_024, 500);
        assert_eq!(total_input_budget, 500);
    }

    #[test]
    fn budget_overflow_base_budget_saturates_when_system_prompt_is_too_large() {
        let large_system = "x".repeat(4_096);
        let base_input_budget = compute_base_input_budget(512, 1_024, &large_system, 1_000);
        assert_eq!(base_input_budget, 0);
    }

    #[test]
    fn budget_overflow_large_context_uses_output_cap_aligned_reserve() {
        let reserve = reserved_output_tokens(1_047_576, 8_192);
        let total_input_budget = compute_total_input_budget(1_047_576, 8_192, 2_000_000);

        assert!(reserve < 20_000);
        assert!(total_input_budget > 1_000_000);
    }

    #[test]
    fn history_overflow_applies_recent_window_cap_even_with_large_budget() {
        let messages: Vec<Message> = (0..200).map(|i| msg(&format!("m{i}"))).collect();
        let assembled = assemble_recent_history(&messages, 100_000);
        assert_eq!(assembled.messages.len(), 20);
        assert_eq!(assembled.dropped_messages, 0);
        assert_eq!(assembled.messages[0].content, "m180");
        assert_eq!(assembled.messages[19].content, "m199");
    }

    #[test]
    fn history_turn_limit_keeps_latest_user_turn_suffix() {
        let mut messages = vec![
            msg("u1"),
            assistant_msg("a1"),
            msg("u2"),
            assistant_msg("a2"),
            msg("u3"),
            assistant_msg("a3"),
        ];

        let dropped = enforce_history_turn_limit(&mut messages, 2);

        assert_eq!(dropped, 2);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].content, "u2");
        assert_eq!(messages[3].content, "a3");
    }

    #[test]
    fn history_turn_limit_scope_defaults_are_stable() {
        assert_eq!(default_history_turn_limit_for_scope("main"), 40);
        assert_eq!(default_history_turn_limit_for_scope("per-group"), 30);
        assert_eq!(default_history_turn_limit_for_scope("per-pipe-sender"), 25);
        assert_eq!(default_history_turn_limit_for_scope("per-sender"), 20);
    }

    #[test]
    fn flow_config_override_history_turn_limit_takes_precedence() {
        let flow = tengu_core::config::FlowConfig {
            scope: "per-sender".to_string(),
            reset_mode: "idle".to_string(),
            idle_timeout_minutes: 30,
            max_history_turns: Some(42),
            compaction_threshold_ratio: None,
            compaction_keep_turns: None,
            compaction_summary_max_tokens: None,
        };

        assert_eq!(resolve_history_turn_limit(&flow), 42);
    }

    #[test]
    fn compaction_split_index_keeps_recent_turn_suffix() {
        let messages = vec![
            msg("u1"),
            assistant_msg("a1"),
            msg("u2"),
            assistant_msg("a2"),
            msg("u3"),
            assistant_msg("a3"),
        ];

        assert_eq!(compaction_split_index(&messages, 2), Some(2));
    }

    #[test]
    fn compaction_policy_defaults_are_scope_aware() {
        assert_eq!(default_compaction_keep_turns_for_scope("main"), 60);
        assert_eq!(default_compaction_keep_turns_for_scope("per-group"), 40);
        assert_eq!(
            default_compaction_keep_turns_for_scope("per-pipe-sender"),
            32
        );
        assert_eq!(default_compaction_keep_turns_for_scope("per-sender"), 24);

        assert!(default_compaction_threshold_ratio_for_scope("main") > 0.85);
        assert!(default_compaction_threshold_ratio_for_scope("per-sender") < 0.85);
    }

    #[test]
    fn resolve_compaction_policy_honors_overrides() {
        let flow = tengu_core::config::FlowConfig {
            scope: "per-sender".to_string(),
            reset_mode: "idle".to_string(),
            idle_timeout_minutes: 30,
            max_history_turns: Some(50),
            compaction_threshold_ratio: Some(0.9),
            compaction_keep_turns: Some(12),
            compaction_summary_max_tokens: Some(300),
        };

        let policy = resolve_flow_compaction_policy(&flow, 1_000, 8_192, 1_024);
        assert_eq!(policy.threshold_tokens, 900);
        assert_eq!(policy.keep_turns, 12);
        assert_eq!(policy.summary_max_tokens, 300);
    }

    #[test]
    fn compaction_summary_default_scales_with_context_budget() {
        let flow = tengu_core::config::FlowConfig {
            scope: "per-sender".to_string(),
            reset_mode: "idle".to_string(),
            idle_timeout_minutes: 30,
            max_history_turns: None,
            compaction_threshold_ratio: None,
            compaction_keep_turns: None,
            compaction_summary_max_tokens: None,
        };

        let small_ctx = resolve_flow_compaction_policy(&flow, 500_000, 8_192, 1_024);
        let large_ctx = resolve_flow_compaction_policy(&flow, 500_000, 128_000, 8_192);

        assert!(small_ctx.summary_max_tokens >= 128);
        assert!(small_ctx.summary_max_tokens < 1_500);
        assert!(large_ctx.summary_max_tokens > small_ctx.summary_max_tokens);
        assert!(large_ctx.summary_max_tokens <= 4_096);
    }

    #[test]
    fn default_config_loads_and_validates() {
        let config = Config::default();
        config.validate().expect("default config should validate");
        assert!(!config.agents.is_empty());
        let (_, agent) = config.agents.iter().next().unwrap();
        assert!(agent.default);
    }
}
