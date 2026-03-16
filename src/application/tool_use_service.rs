use crate::application::ports::{ToolActivityPort, ToolApprovalPort, ToolExecutionPort};
use crate::domain::tool_policy::ToolPolicyCatalog;
use anyhow::Result;
use std::sync::Arc;
use tengu_core::types::ToolCall;

/// Application service for the "execute tool call" use case.
#[derive(Clone)]
pub(crate) struct ToolUseService {
    policies: ToolPolicyCatalog,
    activity: Arc<dyn ToolActivityPort>,
    approval: Arc<dyn ToolApprovalPort>,
    execution: Arc<dyn ToolExecutionPort>,
}

impl ToolUseService {
    pub(crate) fn new(
        policies: ToolPolicyCatalog,
        activity: Arc<dyn ToolActivityPort>,
        approval: Arc<dyn ToolApprovalPort>,
        execution: Arc<dyn ToolExecutionPort>,
    ) -> Self {
        Self {
            policies,
            activity,
            approval,
            execution,
        }
    }

    pub(crate) fn execute(&self, call: &ToolCall) -> Result<String> {
        self.activity.publish_tool_activity(call);

        if !self.policies.is_allowed(&call.name) {
            return Ok(format!(
                "Tool '{}' is not available to this agent.",
                call.name
            ));
        }

        let needs_approval =
            self.policies.requires_approval(&call.name) && !is_read_only_call(call);
        if needs_approval && !self.approval.request_tool_approval(call)? {
            return Ok("Tool execution denied by user.".to_string());
        }

        self.execution.execute_tool(call)
    }
}

/// A tool call is read-only if it carries a `method` argument equal to "GET".
/// API-style skill tools use this convention — reads don't need user approval.
fn is_read_only_call(call: &ToolCall) -> bool {
    call.arguments
        .get("method")
        .and_then(|v| v.as_str())
        .map(|m| m.eq_ignore_ascii_case("GET"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{ToolActivityPort, ToolApprovalPort, ToolExecutionPort};
    use crate::domain::capability::{CapabilityId, EffectClass, RegisteredTool};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tengu_core::types::ToolCall;

    struct TestActivity {
        calls: Arc<AtomicUsize>,
    }
    impl ToolActivityPort for TestActivity {
        fn publish_tool_activity(&self, _call: &ToolCall) {
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct TestApproval {
        allow: bool,
        calls: Arc<AtomicUsize>,
    }
    impl ToolApprovalPort for TestApproval {
        fn request_tool_approval(&self, _call: &ToolCall) -> Result<bool> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.allow)
        }
    }

    struct TestExecution {
        called: Arc<AtomicBool>,
    }
    impl ToolExecutionPort for TestExecution {
        fn execute_tool(&self, _call: &ToolCall) -> Result<String> {
            self.called.store(true, Ordering::Relaxed);
            Ok("ok".into())
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: json!({}),
        }
    }

    fn tool(name: &str, effect_class: EffectClass) -> RegisteredTool {
        let capability = match name {
            "read_file" => "workspace.read",
            "write_file" => "workspace.write",
            _ => "workspace.shell",
        };
        RegisteredTool::new(
            name,
            "",
            json!({}),
            CapabilityId::new(capability).unwrap(),
            effect_class,
        )
    }

    #[test]
    fn service_requests_approval_for_sensitive_tool() {
        let activity_calls = Arc::new(AtomicUsize::new(0));
        let approval_calls = Arc::new(AtomicUsize::new(0));
        let execution_called = Arc::new(AtomicBool::new(false));
        let service = ToolUseService::new(
            ToolPolicyCatalog::from_tools(&[tool("write_file", EffectClass::Write)]),
            Arc::new(TestActivity {
                calls: activity_calls.clone(),
            }),
            Arc::new(TestApproval {
                allow: false,
                calls: approval_calls.clone(),
            }),
            Arc::new(TestExecution {
                called: execution_called.clone(),
            }),
        );

        let result = service.execute(&call("write_file")).unwrap();
        assert_eq!(result, "Tool execution denied by user.");
        assert_eq!(activity_calls.load(Ordering::Relaxed), 1);
        assert_eq!(approval_calls.load(Ordering::Relaxed), 1);
        assert!(!execution_called.load(Ordering::Relaxed));
    }

    #[test]
    fn service_executes_tool_without_approval_when_policy_allows() {
        let service = ToolUseService::new(
            ToolPolicyCatalog::from_tools(&[tool("read_file", EffectClass::Read)]),
            Arc::new(TestActivity {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(TestApproval {
                allow: false,
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(TestExecution {
                called: Arc::new(AtomicBool::new(false)),
            }),
        );

        let result = service.execute(&call("read_file")).unwrap();
        assert_eq!(result, "ok");
    }

    #[test]
    fn service_denies_unknown_tool_even_if_executor_supports_it() {
        let service = ToolUseService::new(
            ToolPolicyCatalog::from_tools(&[tool("read_file", EffectClass::Read)]),
            Arc::new(TestActivity {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(TestApproval {
                allow: true,
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(TestExecution {
                called: Arc::new(AtomicBool::new(false)),
            }),
        );

        let result = service.execute(&call("run_command")).unwrap();
        assert_eq!(result, "Tool 'run_command' is not available to this agent.");
    }

    #[test]
    fn service_skips_approval_for_get_requests() {
        let approval_calls = Arc::new(AtomicUsize::new(0));
        let execution_called = Arc::new(AtomicBool::new(false));
        let service = ToolUseService::new(
            ToolPolicyCatalog::from_tools(&[tool("privy", EffectClass::ChainTx)]),
            Arc::new(TestActivity {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(TestApproval {
                allow: false,
                calls: approval_calls.clone(),
            }),
            Arc::new(TestExecution {
                called: execution_called.clone(),
            }),
        );

        // GET request: should skip approval and execute directly.
        let get_call = ToolCall {
            id: "1".into(),
            name: "privy".into(),
            arguments: json!({"method": "GET", "path": "/v1/wallets", "body": "{}"}),
        };
        let result = service.execute(&get_call).unwrap();
        assert_eq!(result, "ok");
        assert_eq!(approval_calls.load(Ordering::Relaxed), 0);
        assert!(execution_called.load(Ordering::Relaxed));
    }

    #[test]
    fn service_requires_approval_for_post_requests() {
        let approval_calls = Arc::new(AtomicUsize::new(0));
        let service = ToolUseService::new(
            ToolPolicyCatalog::from_tools(&[tool("privy", EffectClass::ChainTx)]),
            Arc::new(TestActivity {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(TestApproval {
                allow: false,
                calls: approval_calls.clone(),
            }),
            Arc::new(TestExecution {
                called: Arc::new(AtomicBool::new(false)),
            }),
        );

        // POST request: should require approval.
        let post_call = ToolCall {
            id: "1".into(),
            name: "privy".into(),
            arguments: json!({"method": "POST", "path": "/v1/wallets", "body": "{}"}),
        };
        let result = service.execute(&post_call).unwrap();
        assert_eq!(result, "Tool execution denied by user.");
        assert_eq!(approval_calls.load(Ordering::Relaxed), 1);
    }
}
