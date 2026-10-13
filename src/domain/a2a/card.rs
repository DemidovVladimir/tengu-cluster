//! tengu's own Agent Card (spec § 8): one per served endpoint — the planner
//! front door (`/a2a`, a skill per routable agent) or one exposed agent
//! (`/a2a/agents/<name>`, its one skill).
//!
//! | Field | Value |
//! |---|---|
//! | `supportedInterfaces` | JSON-RPC 1.0, then JSON-RPC 0.3, both at the endpoint URL |
//! | `capabilities` | `streaming: true`, `pushNotifications: false`, `extendedAgentCard: false` |
//! | `securitySchemes` / `securityRequirements` | `bearer` (HTTP `Bearer`) when the server has a token; none on an unauthenticated loopback server |
//! | `defaultInputModes` / `defaultOutputModes` | `text/plain`, `application/json` / `text/plain` |
//! | 0.3 fields (`url`, `protocolVersion: "0.3"`, `preferredTransport`) | the same endpoint, for 0.3 clients that read a v1.0 card leniently; strict 0.3 clients read `v03::card_to_v03` (`/.well-known/agent.json`, or `A2A-Version: 0.3`) |

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::model::{
    AgentCapabilities, AgentCard, AgentInterface, AgentSkill, JSONRPC, LEGACY_VERSION,
    PROTOCOL_VERSION,
};

/// Input modes tengu accepts (text and JSON data parts).
pub const INPUT_MODES: [&str; 2] = ["text/plain", "application/json"];
/// Output mode tengu answers in (one text artifact).
pub const OUTPUT_MODE: &str = "text/plain";
/// Name of the bearer security scheme.
pub const BEARER: &str = "bearer";

/// What a card says about one endpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct CardSpec {
    pub name: String,
    pub description: String,
    /// The agent's version (tengu's crate version).
    pub version: String,
    /// The JSON-RPC endpoint (absolute URL).
    pub endpoint_url: String,
    /// The endpoint wants `Authorization: Bearer <token>`.
    pub bearer: bool,
    pub skills: Vec<AgentSkill>,
}

/// One skill per tengu agent: id and name = the agent's name, its
/// `description`, its `example_queries`.
pub fn agent_skill(name: &str, description: &str, examples: &[String]) -> AgentSkill {
    AgentSkill {
        id: name.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        tags: vec!["tengu".into(), name.to_string()],
        examples: examples.to_vec(),
        ..Default::default()
    }
}

/// The card of one endpoint (module table).
pub fn build(spec: &CardSpec) -> AgentCard {
    let interface = |version: &str| AgentInterface {
        url: spec.endpoint_url.clone(),
        protocol_binding: JSONRPC.into(),
        tenant: None,
        protocol_version: version.into(),
    };
    let (schemes, requirements) = if spec.bearer {
        let mut m = BTreeMap::new();
        m.insert(
            BEARER.to_string(),
            json!({"httpAuthSecurityScheme": {
                "scheme": "Bearer",
                "description": "Authorization: Bearer <token> — the token the tengu operator issued",
            }}),
        );
        (
            Some(m),
            Some(vec![json!({"schemes": {BEARER: {"list": []}}})]),
        )
    } else {
        (None, None)
    };
    AgentCard {
        name: spec.name.clone(),
        description: spec.description.clone(),
        supported_interfaces: vec![interface(PROTOCOL_VERSION), interface(LEGACY_VERSION)],
        provider: None,
        version: spec.version.clone(),
        documentation_url: None,
        capabilities: AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(false),
            extensions: Vec::new(),
            extended_agent_card: Some(false),
            state_transition_history: None,
        },
        security_schemes: schemes,
        security_requirements: requirements,
        default_input_modes: INPUT_MODES.iter().map(|s| s.to_string()).collect(),
        default_output_modes: vec![OUTPUT_MODE.into()],
        skills: spec.skills.clone(),
        signatures: None,
        icon_url: None,
        url: Some(spec.endpoint_url.clone()),
        // `Major.Minor` only (spec § 3.6); `card_to_v03` writes the 0.3 card's own `0.3.0`.
        protocol_version: Some("0.3".into()),
        preferred_transport: Some(JSONRPC.into()),
        additional_interfaces: None,
    }
}

/// `ETag` of a card: a quoted hex hash of its JSON (spec § 8.6.1).
pub fn etag(card_json: &Value) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(card_json.to_string().as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::a2a::model::Dialect;

    fn spec(bearer: bool) -> CardSpec {
        CardSpec {
            name: "tengu lping".into(),
            description: "d".into(),
            version: "0.1.0".into(),
            endpoint_url: "https://h.example.com/a2a".into(),
            bearer,
            skills: vec![agent_skill("researcher", "Finds things.", &[])],
        }
    }

    #[test]
    fn the_card_offers_v1_then_v03_at_one_url() {
        let card = build(&spec(true));
        let e = card.endpoint().unwrap();
        assert_eq!(
            (e.url.as_str(), e.dialect),
            ("https://h.example.com/a2a", Dialect::V1)
        );
        assert_eq!(card.supported_interfaces[1].protocol_version, "0.3");
        assert_eq!(card.required_schemes(), vec!["bearer".to_string()]);
        let v = serde_json::to_value(&card).unwrap();
        assert_eq!(v["capabilities"]["streaming"], true);
        assert_eq!(v["url"], "https://h.example.com/a2a");
        assert_eq!(v["skills"][0]["id"], "researcher");
    }

    #[test]
    fn an_open_card_has_no_security() {
        let v = serde_json::to_value(build(&spec(false))).unwrap();
        assert!(v.get("securitySchemes").is_none());
        assert!(v.get("securityRequirements").is_none());
    }

    #[test]
    fn etag_follows_the_content() {
        let a = serde_json::to_value(build(&spec(true))).unwrap();
        let b = serde_json::to_value(build(&spec(false))).unwrap();
        assert_ne!(etag(&a), etag(&b));
        assert_eq!(etag(&a), etag(&a));
    }
}
