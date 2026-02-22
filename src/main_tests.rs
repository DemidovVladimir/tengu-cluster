//! Extracted tests for binary runtime orchestration.
//!
//! Potential use case:
//! Keep `main.rs` readable by moving broad integration/unit coverage into a
//! dedicated test module without changing runtime behavior.

#[cfg(test)]
mod tests {
    use crate::control_plane_audit::ControlPlaneAuditEvent;
    use crate::runtime_bus;
    use crate::runtime_commands::prune_expired_capability_assignments;
    use crate::runtime_engine::PendingToolCall;
    use crate::runtime_prompt::{
        assemble_recent_history, build_retrieval_block, compute_base_input_budget,
        compute_retrieval_bucket_budget, compute_total_input_budget, reserved_output_tokens,
    };
    use crate::*;
    use futures::StreamExt;
    use std::path::PathBuf;
    use tengu_core::config::evaluate_tool_policy;
    use tengu_core::events::{
        EventBusOverflowPolicy, HandoffResultReceived, HandoffTaskDispatched,
    };
    use tengu_core::types::{
        HandoffResultEnvelope, HandoffResultStatus, HandoffTaskEnvelope, HANDOFF_SCHEMA_VERSION,
    };

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

    /// Build a deterministic retrieval hit fixture for retrieval budget tests.
    ///
    /// The helper defaults to `is_summary = true` and `score = 1.0` so tests
    /// can focus on token packing/drop behavior instead of ranking variance.
    fn hit(source: &str, content: &str) -> RetrievedKnowledge {
        RetrievedKnowledge {
            source: PathBuf::from(source),
            content: content.to_string(),
            is_summary: true,
            score: 1.0,
        }
    }

    /// Build a deterministic domain event fixture for bus profile validation tests.
    fn runtime_event(seq: &str) -> DomainEvent {
        DomainEvent {
            meta: DomainEventMeta {
                ts_epoch_ms: 1,
                flow_key: Some(format!("flow-{seq}")),
                agent_id: Some("main".to_string()),
                correlation_id: Some(format!("corr-{seq}")),
                source: Some("test".to_string()),
            },
            payload: DomainEventPayload::FlowResolved(FlowResolved {
                flow_key: format!("flow-{seq}"),
                reused_existing: false,
            }),
        }
    }

    #[test]
    fn event_bus_profile_config_is_scope_appropriate() {
        let minimal = runtime_bus::resolve_event_bus_runtime_config(RuntimeProfile::Minimal);
        let desktop = runtime_bus::resolve_event_bus_runtime_config(RuntimeProfile::Desktop);
        let cloud = runtime_bus::resolve_event_bus_runtime_config(RuntimeProfile::Cloud);

        assert_eq!(minimal.policy, EventBusOverflowPolicy::DropNewest);
        assert_eq!(desktop.policy, EventBusOverflowPolicy::DropOldest);
        assert_eq!(cloud.policy, EventBusOverflowPolicy::DropOldest);

        assert!(minimal.capacity < desktop.capacity);
        assert!(desktop.capacity < cloud.capacity);
        assert!(cloud.diagnostics_interval_secs < desktop.diagnostics_interval_secs);
    }

    #[tokio::test]
    async fn event_bus_profile_validation_minimal_tracks_drop_newest_under_pressure() {
        let cfg = runtime_bus::resolve_event_bus_runtime_config(RuntimeProfile::Minimal);
        let bus = InProcessEventBus::new(1, cfg.policy);
        let mut subscriber = bus.subscribe();

        bus.publish(runtime_event("a")).await.expect("publish a");
        bus.publish(runtime_event("b")).await.expect("publish b");

        let _ = subscriber.next().await.expect("consume one");
        let snapshot = bus.diagnostics_snapshot();
        assert!(
            snapshot.dropped_newest_total >= 1,
            "minimal profile should drop newest under saturation"
        );
    }

    #[tokio::test]
    async fn event_bus_profile_validation_desktop_tracks_lag_for_drop_oldest() {
        let cfg = runtime_bus::resolve_event_bus_runtime_config(RuntimeProfile::Desktop);
        let bus = InProcessEventBus::new(2, cfg.policy);
        let mut subscriber = bus.subscribe();

        bus.publish(runtime_event("1")).await.expect("publish 1");
        bus.publish(runtime_event("2")).await.expect("publish 2");
        bus.publish(runtime_event("3")).await.expect("publish 3");
        bus.publish(runtime_event("4")).await.expect("publish 4");

        let _ = subscriber.next().await.expect("receive latest");
        let snapshot = bus.diagnostics_snapshot();
        assert!(
            snapshot.lagged_events_total >= 1,
            "desktop profile should record lag when receivers fall behind"
        );
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
    fn retrieval_drop_policy_drops_tail_after_first_overflow() {
        let first = hit("a.md", &"alpha".repeat(24));
        let second = hit("b.md", &"beta".repeat(24));
        let first_section = format!(
            "[{} | {} | score {:.2}]\n{}",
            first.source.display(),
            "summary",
            first.score,
            first.content
        );
        let budget = estimate_tokens_approx_min1(RETRIEVAL_CONTEXT_HEADER)
            .saturating_add(estimate_tokens_approx_min1(&first_section))
            .saturating_add(1);

        let assembled = build_retrieval_block(&[first, second], budget);
        let block = assembled.block.unwrap_or_default();

        assert!(block.contains("a.md"));
        assert!(!block.contains("b.md"));
        assert_eq!(assembled.dropped_items, 1);
        assert!(assembled.used_tokens <= budget);
    }

    #[test]
    fn retrieval_drop_policy_skips_individually_oversized_entries() {
        let huge = hit("huge.md", &"x".repeat(2_000));
        let small = hit("small.md", &"y".repeat(160));
        let small_section = format!(
            "[{} | {} | score {:.2}]\n{}",
            small.source.display(),
            "summary",
            small.score,
            small.content
        );
        let budget = estimate_tokens_approx_min1(RETRIEVAL_CONTEXT_HEADER)
            .saturating_add(estimate_tokens_approx_min1(&small_section))
            .saturating_add(2);

        let assembled = build_retrieval_block(&[huge, small], budget);
        let block = assembled.block.unwrap_or_default();

        assert!(block.contains("small.md"));
        assert!(!block.contains("huge.md"));
        assert_eq!(assembled.dropped_items, 1);
        assert!(assembled.used_tokens <= budget);
    }

    #[test]
    fn budget_overflow_small_context_window_preserves_output_reserve() {
        // For tiny windows, capped output reserve can consume all available input.
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
        let base_input_budget = compute_base_input_budget(512, 1_024, Some(&large_system), 1_000);
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
    fn budget_overflow_retrieval_bucket_respects_half_input_hard_cap() {
        let lens_cfg = tengu_core::config::LensConfig {
            eco_max_tokens: 10_000,
            standard_threshold: 0.7,
            precise_budget: 0.9,
        };
        let budget = compute_retrieval_bucket_budget(Lens::Eco, &lens_cfg, 100);
        assert_eq!(budget, 50);
    }

    #[test]
    fn history_overflow_applies_recent_window_cap_even_with_large_budget() {
        let messages: Vec<Message> = (0..200).map(|i| msg(&format!("m{i}"))).collect();
        let assembled = assemble_recent_history(&messages, 100_000);
        assert_eq!(assembled.messages.len(), 120);
        // `dropped_messages` is tracked within the 120-message recent window.
        assert_eq!(assembled.dropped_messages, 0);
        assert_eq!(assembled.messages[0].content, "m80");
        assert_eq!(assembled.messages[119].content, "m199");
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
        assert_eq!(default_history_turn_limit_for_scope("main"), 160);
        assert_eq!(default_history_turn_limit_for_scope("per-group"), 120);
        assert_eq!(default_history_turn_limit_for_scope("per-pipe-sender"), 100);
        assert_eq!(default_history_turn_limit_for_scope("per-sender"), 80);
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

    #[tokio::test]
    async fn execute_tool_call_reports_unregistered_tool() {
        let registry = ToolRegistry::with_defaults();
        let config = tengu_core::config::Config::default();
        let agent_config = config.agents.get("main").expect("main").clone();
        let pending = PendingToolCall {
            id: "tool-1".to_string(),
            name: "shell".to_string(),
            arguments_delta: "{\"command\":\"ls\"}".to_string(),
        };

        let outcome = runtime_engine::execute_tool_call(
            &pending,
            &config,
            &agent_config,
            &registry,
            Some(&PathBuf::from(".")),
            "main",
        )
        .await;
        assert_eq!(outcome.status, "not_registered");
        assert!(outcome.user_message.contains("not registered"));
    }

    #[tokio::test]
    async fn execute_tool_call_runs_read_file_tool() {
        let workspace =
            std::env::temp_dir().join(format!("tengu-main-tool-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("create temp workspace");
        std::fs::write(workspace.join("note.txt"), "hello from tool").expect("write fixture file");

        let registry = ToolRegistry::with_defaults();
        let config = tengu_core::config::Config::default();
        let mut agent_config = config.agents.get("main").expect("main").clone();
        agent_config.kit.approved = vec!["read_file".to_string()];
        let pending = PendingToolCall {
            id: "tool-2".to_string(),
            name: "read_file".to_string(),
            arguments_delta: "{\"path\":\"note.txt\"}".to_string(),
        };
        let outcome = runtime_engine::execute_tool_call(
            &pending,
            &config,
            &agent_config,
            &registry,
            Some(&workspace),
            "main",
        )
        .await;

        assert_eq!(outcome.status, "ok");
        assert!(outcome.user_message.contains("[tool:read_file ok]"));
        assert!(outcome.user_message.contains("hello from tool"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn execute_tool_call_denies_when_approval_required_but_missing() {
        let workspace =
            std::env::temp_dir().join(format!("tengu-main-tool-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("create temp workspace");
        std::fs::write(workspace.join("note.txt"), "hello from tool").expect("write fixture file");

        let registry = ToolRegistry::with_defaults();
        let config = tengu_core::config::Config::default();
        let mut agent_config = config.agents.get("main").expect("main").clone();
        agent_config.kit.approval_required = vec!["read_file".to_string()];
        agent_config.kit.approved.clear();
        let pending = PendingToolCall {
            id: "tool-3".to_string(),
            name: "read_file".to_string(),
            arguments_delta: "{\"path\":\"note.txt\"}".to_string(),
        };
        let outcome = runtime_engine::execute_tool_call(
            &pending,
            &config,
            &agent_config,
            &registry,
            Some(&workspace),
            "main",
        )
        .await;

        assert_eq!(outcome.status, "denied_approval");
        assert!(outcome.user_message.contains("requires approval"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn execute_tool_call_denies_when_policy_blocks_tool() {
        let workspace =
            std::env::temp_dir().join(format!("tengu-main-tool-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("create temp workspace");
        std::fs::write(workspace.join("note.txt"), "hello from tool").expect("write fixture file");

        let registry = ToolRegistry::with_defaults();
        let config = tengu_core::config::Config::default();
        let mut agent_config = config.agents.get("main").expect("main").clone();
        agent_config.kit.deny = vec!["read_file".to_string()];
        agent_config.kit.approval_required.clear();
        agent_config.kit.approved = vec!["read_file".to_string()];
        let pending = PendingToolCall {
            id: "tool-4".to_string(),
            name: "read_file".to_string(),
            arguments_delta: "{\"path\":\"note.txt\"}".to_string(),
        };
        let outcome = runtime_engine::execute_tool_call(
            &pending,
            &config,
            &agent_config,
            &registry,
            Some(&workspace),
            "main",
        )
        .await;

        assert_eq!(outcome.status, "denied_policy");
        assert!(outcome.user_message.contains("blocked by policy"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn execute_tool_call_denies_non_orchestrator_in_delegated_mode() {
        let workspace =
            std::env::temp_dir().join(format!("tengu-main-tool-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("create temp workspace");
        std::fs::write(workspace.join("note.txt"), "hello from tool").expect("write fixture file");

        let registry = ToolRegistry::with_defaults();
        let mut config = tengu_core::config::Config::default();
        config.capability_governance.mode = "delegated".to_string();
        config.capability_governance.delegated_orchestrator_agent =
            Some("orchestrator".to_string());

        let mut agent_config = config.agents.get("main").expect("main").clone();
        agent_config.kit.approved = vec!["read_file".to_string()];
        let pending = PendingToolCall {
            id: "tool-5".to_string(),
            name: "read_file".to_string(),
            arguments_delta: "{\"path\":\"note.txt\"}".to_string(),
        };
        let outcome = runtime_engine::execute_tool_call(
            &pending,
            &config,
            &agent_config,
            &registry,
            Some(&workspace),
            "main",
        )
        .await;

        assert_eq!(outcome.status, "denied_governance");
        assert!(outcome
            .user_message
            .contains("Capability governance denied"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn tool_policy_denies_blocked_tool_name() {
        let mut config = tengu_core::config::Config::default();
        let agent = config.agents.get_mut("main").expect("main");
        agent.kit.allow.clear();
        agent.kit.deny = vec!["shell".to_string()];

        let decision = evaluate_tool_policy(agent, "shell");
        assert!(!decision.is_allowed());
    }

    #[test]
    fn control_plane_audit_map_captures_handoff_dispatch() {
        let event = DomainEvent {
            meta: DomainEventMeta {
                ts_epoch_ms: 1_000,
                flow_key: Some("ignored-meta-flow".to_string()),
                agent_id: Some("main".to_string()),
                correlation_id: Some("corr-1".to_string()),
                source: Some("test".to_string()),
            },
            payload: DomainEventPayload::HandoffTaskDispatched(HandoffTaskDispatched {
                envelope: tengu_core::types::HandoffTaskEnvelope {
                    schema_version: HANDOFF_SCHEMA_VERSION,
                    handoff_id: "handoff-1".to_string(),
                    flow_key: "flow-1".to_string(),
                    from_agent_id: "main".to_string(),
                    to_agent_id: "worker".to_string(),
                    objective: "Inspect one file".to_string(),
                    constraints: vec!["bounded-by-user-policy".to_string()],
                    requested_capabilities: vec!["tool:read_file".to_string()],
                    context_summary: None,
                    max_output_tokens: 256,
                    ttl_seconds: Some(300),
                    metadata: std::collections::HashMap::new(),
                },
            }),
        };

        let mapped =
            runtime_bus::map_domain_event_to_control_plane_audit_event(&event).expect("mapped");
        assert_eq!(mapped.status, "approved");
        assert_eq!(mapped.flow_key, "flow-1");
        assert_eq!(mapped.orchestrator_agent_id, "main");
        assert_eq!(mapped.dependent_agent_id, "worker");
    }

    #[test]
    fn control_plane_audit_map_ignores_handoff_acceptance() {
        let event = DomainEvent {
            meta: DomainEventMeta {
                ts_epoch_ms: 1_500,
                flow_key: Some("flow-1".to_string()),
                agent_id: Some("worker".to_string()),
                correlation_id: Some("corr-1b".to_string()),
                source: Some("test".to_string()),
            },
            payload: DomainEventPayload::HandoffResultReceived(HandoffResultReceived {
                envelope: HandoffResultEnvelope {
                    schema_version: HANDOFF_SCHEMA_VERSION,
                    handoff_id: "handoff-1".to_string(),
                    flow_key: "flow-1".to_string(),
                    from_agent_id: "worker".to_string(),
                    to_agent_id: "main".to_string(),
                    status: HandoffResultStatus::Accepted,
                    summary: "accepted".to_string(),
                    artifacts: Vec::new(),
                    output_tokens: 0,
                    error_reason: None,
                    metadata: std::collections::HashMap::new(),
                },
            }),
        };

        assert!(runtime_bus::map_domain_event_to_control_plane_audit_event(&event).is_none());
    }

    #[test]
    fn control_plane_audit_map_captures_handoff_denial() {
        let event = DomainEvent {
            meta: DomainEventMeta {
                ts_epoch_ms: 2_000,
                flow_key: Some("flow-1".to_string()),
                agent_id: Some("main".to_string()),
                correlation_id: Some("corr-2".to_string()),
                source: Some("test".to_string()),
            },
            payload: DomainEventPayload::HandoffResultReceived(HandoffResultReceived {
                envelope: HandoffResultEnvelope {
                    schema_version: HANDOFF_SCHEMA_VERSION,
                    handoff_id: "handoff-2".to_string(),
                    flow_key: "flow-1".to_string(),
                    from_agent_id: "worker".to_string(),
                    to_agent_id: "main".to_string(),
                    status: HandoffResultStatus::Denied,
                    summary: String::new(),
                    artifacts: Vec::new(),
                    output_tokens: 0,
                    error_reason: Some("policy denied".to_string()),
                    metadata: std::collections::HashMap::new(),
                },
            }),
        };

        let mapped =
            runtime_bus::map_domain_event_to_control_plane_audit_event(&event).expect("mapped");
        assert_eq!(mapped.status, "denied");
        assert_eq!(mapped.reason.as_deref(), Some("policy denied"));
        assert_eq!(mapped.orchestrator_agent_id, "main");
        assert_eq!(mapped.dependent_agent_id, "worker");
    }

    #[test]
    fn control_plane_audit_map_captures_handoff_revocation() {
        let event = DomainEvent {
            meta: DomainEventMeta {
                ts_epoch_ms: 3_000,
                flow_key: Some("flow-1".to_string()),
                agent_id: Some("main".to_string()),
                correlation_id: Some("corr-3".to_string()),
                source: Some("test".to_string()),
            },
            payload: DomainEventPayload::HandoffResultReceived(HandoffResultReceived {
                envelope: HandoffResultEnvelope {
                    schema_version: HANDOFF_SCHEMA_VERSION,
                    handoff_id: "handoff-3".to_string(),
                    flow_key: "flow-1".to_string(),
                    from_agent_id: "worker".to_string(),
                    to_agent_id: "main".to_string(),
                    status: HandoffResultStatus::Denied,
                    summary: String::new(),
                    artifacts: Vec::new(),
                    output_tokens: 0,
                    error_reason: Some("revoked by orchestrator via /unassign".to_string()),
                    metadata: std::collections::HashMap::new(),
                },
            }),
        };

        let mapped =
            runtime_bus::map_domain_event_to_control_plane_audit_event(&event).expect("mapped");
        assert_eq!(mapped.status, "revoked");
    }

    #[test]
    fn control_plane_audit_map_captures_handoff_expiry() {
        let event = DomainEvent {
            meta: DomainEventMeta {
                ts_epoch_ms: 4_000,
                flow_key: Some("flow-1".to_string()),
                agent_id: Some("main".to_string()),
                correlation_id: Some("corr-4".to_string()),
                source: Some("test".to_string()),
            },
            payload: DomainEventPayload::HandoffResultReceived(HandoffResultReceived {
                envelope: HandoffResultEnvelope {
                    schema_version: HANDOFF_SCHEMA_VERSION,
                    handoff_id: "handoff-4".to_string(),
                    flow_key: "flow-1".to_string(),
                    from_agent_id: "worker".to_string(),
                    to_agent_id: "main".to_string(),
                    status: HandoffResultStatus::Denied,
                    summary: String::new(),
                    artifacts: Vec::new(),
                    output_tokens: 0,
                    error_reason: Some("expired by retention policy".to_string()),
                    metadata: std::collections::HashMap::new(),
                },
            }),
        };

        let mapped =
            runtime_bus::map_domain_event_to_control_plane_audit_event(&event).expect("mapped");
        assert_eq!(mapped.status, "expired");
    }

    #[test]
    fn load_persisted_capability_assignments_replays_recent_approved_for_actor() {
        let home = std::env::temp_dir().join(format!("tengu-replay-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).expect("home");
        let store = ControlPlaneAuditStore::new(&home).expect("store");
        let now_s = now_epoch_ms() / 1_000;

        store
            .append(&ControlPlaneAuditEvent {
                ts_epoch_s: now_s.saturating_sub(4),
                flow_key: "flow-1".to_string(),
                handoff_id: "h-1".to_string(),
                orchestrator_agent_id: "main".to_string(),
                dependent_agent_id: "worker-a".to_string(),
                status: "approved".to_string(),
                reason: None,
                requested_capabilities: vec!["tool:read_file".to_string()],
                objective: Some("A".to_string()),
            })
            .expect("append 1");
        store
            .append(&ControlPlaneAuditEvent {
                ts_epoch_s: now_s.saturating_sub(3),
                flow_key: "flow-2".to_string(),
                handoff_id: "h-2".to_string(),
                orchestrator_agent_id: "other".to_string(),
                dependent_agent_id: "worker-b".to_string(),
                status: "approved".to_string(),
                reason: None,
                requested_capabilities: vec!["tool:search_content".to_string()],
                objective: Some("B".to_string()),
            })
            .expect("append 2");
        store
            .append(&ControlPlaneAuditEvent {
                ts_epoch_s: now_s.saturating_sub(2),
                flow_key: "flow-3".to_string(),
                handoff_id: "h-1".to_string(),
                orchestrator_agent_id: "main".to_string(),
                dependent_agent_id: "worker-a".to_string(),
                status: "denied".to_string(),
                reason: Some("closed".to_string()),
                requested_capabilities: Vec::new(),
                objective: None,
            })
            .expect("append 3");
        store
            .append(&ControlPlaneAuditEvent {
                ts_epoch_s: now_s.saturating_sub(1),
                flow_key: "flow-4".to_string(),
                handoff_id: "h-4".to_string(),
                orchestrator_agent_id: "main".to_string(),
                dependent_agent_id: "worker-c".to_string(),
                status: "approved".to_string(),
                reason: None,
                requested_capabilities: vec!["skill:analysis".to_string()],
                objective: Some("C".to_string()),
            })
            .expect("append 4");

        let recovered =
            runtime_bus::load_persisted_capability_assignments(Some(&store), "main", 64, 86_400);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].handoff_id, "h-4");
        assert_eq!(recovered[0].dependent_agent_id, "worker-c");

        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn load_persisted_capability_assignments_skips_expired_rows() {
        let home =
            std::env::temp_dir().join(format!("tengu-replay-expiry-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).expect("home");
        let store = ControlPlaneAuditStore::new(&home).expect("store");

        store
            .append(&ControlPlaneAuditEvent {
                ts_epoch_s: 1,
                flow_key: "flow-old".to_string(),
                handoff_id: "h-old".to_string(),
                orchestrator_agent_id: "main".to_string(),
                dependent_agent_id: "worker-a".to_string(),
                status: "approved".to_string(),
                reason: None,
                requested_capabilities: vec!["tool:read_file".to_string()],
                objective: Some("old".to_string()),
            })
            .expect("append old");

        let recovered =
            runtime_bus::load_persisted_capability_assignments(Some(&store), "main", 64, 1);
        assert!(recovered.is_empty());

        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn build_handoff_accepted_result_is_valid_against_task() {
        let task = HandoffTaskEnvelope {
            schema_version: HANDOFF_SCHEMA_VERSION,
            handoff_id: "h-accepted".to_string(),
            flow_key: "flow-accepted".to_string(),
            from_agent_id: "main".to_string(),
            to_agent_id: "worker".to_string(),
            objective: "check one file".to_string(),
            constraints: vec!["bounded-by-user-policy".to_string()],
            requested_capabilities: vec!["tool:read_file".to_string()],
            context_summary: None,
            max_output_tokens: 256,
            ttl_seconds: Some(900),
            metadata: std::collections::HashMap::new(),
        };
        let accepted = runtime_bus::build_handoff_accepted_result(&task, "accepted");
        accepted.validate_against(&task).expect("valid accepted");
        assert_eq!(accepted.status, HandoffResultStatus::Accepted);
        assert_eq!(
            accepted.metadata.get("runtime").map(String::as_str),
            Some("delegated-handoff-queue")
        );
    }

    #[test]
    fn build_handoff_failed_result_is_valid_against_task() {
        let task = HandoffTaskEnvelope {
            schema_version: HANDOFF_SCHEMA_VERSION,
            handoff_id: "h-failed".to_string(),
            flow_key: "flow-failed".to_string(),
            from_agent_id: "main".to_string(),
            to_agent_id: "worker".to_string(),
            objective: "check one file".to_string(),
            constraints: vec!["bounded-by-user-policy".to_string()],
            requested_capabilities: vec!["tool:read_file".to_string()],
            context_summary: Some("ctx".to_string()),
            max_output_tokens: 256,
            ttl_seconds: Some(900),
            metadata: std::collections::HashMap::new(),
        };
        let failed = runtime_bus::build_handoff_failed_result(&task, "runtime failed");
        failed.validate_against(&task).expect("valid failed");
        assert_eq!(failed.status, HandoffResultStatus::Failed);
        assert_eq!(failed.error_reason.as_deref(), Some("runtime failed"));
    }

    #[test]
    fn build_delegated_handoff_prompt_contains_objective_and_bounds() {
        let task = HandoffTaskEnvelope {
            schema_version: HANDOFF_SCHEMA_VERSION,
            handoff_id: "h-prompt".to_string(),
            flow_key: "flow-prompt".to_string(),
            from_agent_id: "main".to_string(),
            to_agent_id: "worker".to_string(),
            objective: "inspect logs".to_string(),
            constraints: vec!["no external calls".to_string()],
            requested_capabilities: vec!["tool:read_file".to_string()],
            context_summary: Some("recent logs".to_string()),
            max_output_tokens: 128,
            ttl_seconds: Some(120),
            metadata: std::collections::HashMap::new(),
        };
        let prompt = runtime_bus::build_delegated_handoff_prompt(&task);
        assert!(prompt.contains("inspect logs"));
        assert!(prompt.contains("tool:read_file"));
        assert!(prompt.contains("no external calls"));
        assert!(prompt.contains("recent logs"));
    }

    #[tokio::test]
    async fn execute_delegated_handoff_task_once_fails_for_unknown_dependent() {
        let config = Config::default();
        let task = HandoffTaskEnvelope {
            schema_version: HANDOFF_SCHEMA_VERSION,
            handoff_id: "h-unknown".to_string(),
            flow_key: "flow-unknown".to_string(),
            from_agent_id: "main".to_string(),
            to_agent_id: "worker-ghost".to_string(),
            objective: "run".to_string(),
            constraints: vec![],
            requested_capabilities: vec!["tool:read_file".to_string()],
            context_summary: None,
            max_output_tokens: 256,
            ttl_seconds: Some(900),
            metadata: std::collections::HashMap::new(),
        };

        let result = runtime_bus::execute_delegated_handoff_task_once(&config, &task).await;
        assert_eq!(result.status, HandoffResultStatus::Failed);
        assert!(result
            .error_reason
            .as_deref()
            .unwrap_or_default()
            .contains("not configured"));
        result
            .validate_against(&task)
            .expect("failed result is valid");
    }

    #[tokio::test]
    async fn prune_expired_capability_assignments_removes_old_records() {
        let event_bus = InProcessEventBus::new(32, EventBusOverflowPolicy::DropNewest);
        let mut assignments = vec![
            CapabilityAssignmentRecord {
                handoff_id: "h-old".to_string(),
                flow_key: "flow-1".to_string(),
                orchestrator_agent_id: "main".to_string(),
                dependent_agent_id: "worker".to_string(),
                requested_capabilities: vec!["tool:read_file".to_string()],
                objective: "old".to_string(),
                issued_at_epoch_ms: now_epoch_ms().saturating_sub(2_000),
            },
            CapabilityAssignmentRecord {
                handoff_id: "h-new".to_string(),
                flow_key: "flow-1".to_string(),
                orchestrator_agent_id: "main".to_string(),
                dependent_agent_id: "worker".to_string(),
                requested_capabilities: vec!["tool:search_content".to_string()],
                objective: "new".to_string(),
                issued_at_epoch_ms: now_epoch_ms(),
            },
        ];

        let removed =
            prune_expired_capability_assignments(&mut assignments, 1, &event_bus, "main", "corr-1")
                .await;
        assert_eq!(removed, 1);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].handoff_id, "h-new");
    }
}
