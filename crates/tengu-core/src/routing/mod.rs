use crate::config::RoutingBinding;
use crate::types::Recipient;

/// Routes inbound messages to the correct agent.
pub struct Router {
    bindings: Vec<RoutingBinding>,
    default_agent: String,
}

impl Router {
    pub fn new(bindings: Vec<RoutingBinding>, default_agent: String) -> Self {
        Self { bindings, default_agent }
    }

    /// Resolve which agent should handle a message from the given sender.
    /// Uses deterministic matching — most specific binding wins.
    pub fn resolve(&self, sender: &Recipient) -> &str {
        // Priority 1: exact peer match
        for b in &self.bindings {
            if b.pipe == sender.pipe_id {
                if let Some(ref peer) = b.peer {
                    if peer == &sender.peer_id {
                        return &b.agent;
                    }
                }
            }
        }

        // Priority 2: group match
        for b in &self.bindings {
            if b.pipe == sender.pipe_id {
                if let (Some(ref group), Some(ref thread)) = (&b.group_id, &sender.thread_id) {
                    if group == thread {
                        return &b.agent;
                    }
                }
            }
        }

        // Priority 3: account match
        for b in &self.bindings {
            if b.pipe == sender.pipe_id {
                if let (Some(ref acc_b), Some(ref acc_s)) = (&b.account_id, &sender.account_id) {
                    if acc_b == acc_s {
                        return &b.agent;
                    }
                }
            }
        }

        // Priority 4: pipe-level match (no peer/group/account specified)
        for b in &self.bindings {
            if b.pipe == sender.pipe_id && b.peer.is_none() && b.group_id.is_none() && b.account_id.is_none() {
                return &b.agent;
            }
        }

        // Fallback
        &self.default_agent
    }
}
