//! Deterministic sender-to-agent routing.
//!
//! Priority order:
//! 1. `peer`
//! 2. `group_id`
//! 3. `account_id`
//! 4. pipe-only binding
//! 5. default agent fallback
//!
//! Potential use case:
//! Route Telegram user A to `sales-agent` and user B to `support-agent` in one runtime.

use crate::config::RoutingBinding;
use crate::types::Recipient;

/// Router resolving inbound senders to target agent IDs.
pub struct Router {
    /// Candidate routing bindings scanned in deterministic order.
    ///
    /// Within each priority tier, the first matching binding wins.
    bindings: Vec<RoutingBinding>,
    /// Fallback agent used when no binding matches.
    default_agent: String,
}

impl Router {
    /// Build a router from ordered bindings and default fallback agent.
    pub fn new(bindings: Vec<RoutingBinding>, default_agent: String) -> Self {
        Self {
            bindings,
            default_agent,
        }
    }

    /// Resolve sender identity into an agent ID.
    ///
    /// Matching precedence is fixed:
    /// `peer` -> `group_id` -> `account_id` -> pipe-only -> default.
    pub fn resolve(&self, sender: &Recipient) -> &str {
        // Priority order: peer -> group -> account -> pipe-only -> default.
        self.bindings
            .iter()
            .find(|b| Self::matches_peer(b, sender))
            .or_else(|| {
                self.bindings
                    .iter()
                    .find(|b| Self::matches_group(b, sender))
            })
            .or_else(|| {
                self.bindings
                    .iter()
                    .find(|b| Self::matches_account(b, sender))
            })
            .or_else(|| {
                self.bindings
                    .iter()
                    .find(|b| Self::matches_pipe_only(b, sender))
            })
            .map(|b| b.agent.as_str())
            .unwrap_or(self.default_agent.as_str())
    }

    /// Return true when binding and sender belong to the same pipe.
    fn same_pipe(binding: &RoutingBinding, sender: &Recipient) -> bool {
        binding.pipe == sender.pipe_id
    }

    /// Match a binding scoped to an exact peer identifier.
    fn matches_peer(binding: &RoutingBinding, sender: &Recipient) -> bool {
        Self::same_pipe(binding, sender) && binding.peer.as_deref() == Some(sender.peer_id.as_str())
    }

    /// Match a binding scoped to a thread/group identifier.
    fn matches_group(binding: &RoutingBinding, sender: &Recipient) -> bool {
        Self::same_pipe(binding, sender)
            && binding.group_id.as_deref() == sender.thread_id.as_deref()
            && binding.group_id.is_some()
    }

    /// Match a binding scoped to an account identifier.
    fn matches_account(binding: &RoutingBinding, sender: &Recipient) -> bool {
        Self::same_pipe(binding, sender)
            && binding.account_id.as_deref() == sender.account_id.as_deref()
            && binding.account_id.is_some()
    }

    /// Match a binding that targets the whole pipe without extra scoping.
    fn matches_pipe_only(binding: &RoutingBinding, sender: &Recipient) -> bool {
        Self::same_pipe(binding, sender)
            && binding.peer.is_none()
            && binding.group_id.is_none()
            && binding.account_id.is_none()
    }
}
