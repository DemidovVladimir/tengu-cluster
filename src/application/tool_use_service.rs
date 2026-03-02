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

        if self.policies.requires_approval(&call.name)
            && !self.approval.request_tool_approval(call)?
        {
            return Ok("Write denied by user.".to_string());
        }

        self.execution.execute_tool(call)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{ToolActivityPort, ToolApprovalPort, ToolExecutionPort};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tengu_core::types::{ToolCall, ToolDef, ToolPolicyMetadata, ToolRiskLevel};

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

    fn tool(name: &str, requires_approval: bool) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: String::new(),
            parameters: json!({}),
            policy: Some(ToolPolicyMetadata {
                risk_level: ToolRiskLevel::Low,
                requires_approval,
            }),
        }
    }

    #[test]
    fn service_requests_approval_for_sensitive_tool() {
        let activity_calls = Arc::new(AtomicUsize::new(0));
        let approval_calls = Arc::new(AtomicUsize::new(0));
        let execution_called = Arc::new(AtomicBool::new(false));
        let service = ToolUseService::new(
            ToolPolicyCatalog::from_tools(&[tool("write_file", true)]),
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
        assert_eq!(result, "Write denied by user.");
        assert_eq!(activity_calls.load(Ordering::Relaxed), 1);
        assert_eq!(approval_calls.load(Ordering::Relaxed), 1);
        assert!(!execution_called.load(Ordering::Relaxed));
    }

    #[test]
    fn service_executes_tool_without_approval_when_policy_allows() {
        let service = ToolUseService::new(
            ToolPolicyCatalog::from_tools(&[tool("read_file", false)]),
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
}
