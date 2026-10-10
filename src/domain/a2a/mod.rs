//! A2A (Agent2Agent, spec 1.0.1 — `a2aproject/A2A`, Linux Foundation) —
//! the protocol tengu agents use to talk to other agent harnesses: the
//! `a2a` tool calls remote agents (`outbound/a2a/`), `tengu a2a serve`
//! serves tengu's agents (`inbound/a2a.rs`, `application/a2a/`). Pure:
//! wire types, JSON-RPC rules, the 0.3 dialect, card building, text.
//!
//! | File | Holds |
//! |---|---|
//! | `model.rs` | v1.0 types (`Task`, `Message`, `Part`, `AgentCard`, …), interface choice (`AgentCard::endpoint`) |
//! | `rpc.rs` | JSON-RPC methods of both dialects, error codes, envelopes, `A2A-Version` negotiation |
//! | `v03.rs` | 0.3 ↔ v1.0 conversion (tengu serves and calls 0.3 agents too) |
//! | `card.rs` | tengu's own Agent Card per served endpoint |
//! | `render.rs` | text for a model / the operator (ids whole, content capped) |

pub mod card;
pub mod model;
pub mod render;
pub mod rpc;
pub mod v03;

/// `ms` since the epoch as the spec's timestamp (`YYYY-MM-DDTHH:mm:ss.sssZ`).
pub fn iso_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// The spec's timestamp back to ms (`None` = not ISO 8601).
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.timestamp_millis())
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamps_are_utc_millis() {
        assert_eq!(super::iso_ms(1_760_090_400_123), "2025-10-10T10:00:00.123Z");
        assert_eq!(
            super::parse_iso_ms("2025-10-10T10:00:00.123Z"),
            Some(1_760_090_400_123)
        );
        assert_eq!(super::parse_iso_ms("yesterday"), None);
    }
}
