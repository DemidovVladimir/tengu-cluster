//! Output port for publishing tool activity events to the UI/log layer.

use crate::domain::message::ToolCall;

/// Output port for publishing tool activity events to the UI/log layer.
pub(crate) trait ToolActivityPort: Send + Sync {
    fn publish_tool_activity(&self, call: &ToolCall);
}
