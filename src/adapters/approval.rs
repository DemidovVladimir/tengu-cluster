use crate::adapters::ports::ToolApprovalPort;
use anyhow::Result;
use crate::adapters::types::ToolCall;

/// Approval adapter for contexts where no interactive approval UI exists.
///
/// Side-effecting tools fail closed instead of auto-approving.
pub(crate) struct DenyByDefaultApproval;

impl ToolApprovalPort for DenyByDefaultApproval {
    fn request_tool_approval(&self, call: &ToolCall) -> Result<bool> {
        tracing::warn!(tool = %call.name, "No interactive approval adapter available; denying tool call");
        Ok(false)
    }
}

/// Approval adapter for trusted interactive flows where explicit confirmation is disabled.
pub(crate) struct AllowAllApproval;

impl ToolApprovalPort for AllowAllApproval {
    fn request_tool_approval(&self, call: &ToolCall) -> Result<bool> {
        tracing::info!(tool = %call.name, "Auto-approving tool call");
        Ok(true)
    }
}
