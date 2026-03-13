use crate::application::ports::ToolApprovalPort;
use anyhow::Result;
use tengu_core::types::ToolCall;

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
