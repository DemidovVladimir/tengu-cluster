//! Starting blueprints `tengu sandbox new` offers (`--template`). Each one
//! compiles to a config that loads (`tests::every_template_loads`).
//!
//! | Template | Cards |
//! |---|---|
//! | `blank` | Sandbox + one OpenRouter agent (default) |
//! | `team` | Orchestrator → planner → researcher (http_request) + writer (confined workspace); the OpenRouter key |
//! | `telegram` | Telegram bot → assistant; bot token + OpenRouter key |

use serde_json::{json, Map, Value};

use crate::domain::blueprint::{Blueprint, Edge, Node, View, BLUEPRINT_SCHEMA_VERSION};

/// (name, one-line description) of every template, in menu order.
pub(crate) const TEMPLATES: &[(&str, &str)] = &[
    ("blank", "one agent — start from scratch"),
    (
        "team",
        "planner + researcher + writer (orchestrated plan steps)",
    ),
    ("telegram", "a Telegram bot that chats with one agent"),
];

fn node(id: &str, kind: &str, x: f64, y: f64, fields: Value) -> Node {
    Node {
        id: id.into(),
        kind: kind.into(),
        x,
        y,
        fields: match fields {
            Value::Object(m) => m,
            _ => Map::new(),
        },
    }
}

fn edge(id: &str, from: &str, to: &str) -> Edge {
    Edge {
        id: id.into(),
        from: from.into(),
        to: to.into(),
    }
}

fn agent(name: &str, default: bool, instructions: &str) -> Value {
    json!({
        "name": name,
        "engine": "openrouter",
        "model": "anthropic/claude-sonnet-4-6",
        "default": default,
        "instructions": instructions,
    })
}

/// The template `name` for sandbox `sandbox`, `None` for an unknown name.
pub(crate) fn template(name: &str, sandbox: &str) -> Option<Blueprint> {
    let sb = node(
        "n-sandbox",
        "sandbox",
        40.0,
        40.0,
        json!({ "network": "tor", "memory": false }),
    );
    let (nodes, edges) = match name {
        "blank" => (
            vec![
                sb,
                node(
                    "n-agent-assistant",
                    "agent",
                    380.0,
                    200.0,
                    agent("assistant", true, "You are a helpful assistant. Answer briefly."),
                ),
            ],
            vec![],
        ),
        "team" => {
            let ws = format!("~/tengu-work/{sandbox}");
            (
                vec![
                    sb,
                    node("n-orchestrator", "orchestrator", 60.0, 300.0, json!({})),
                    node(
                        "n-agent-planner",
                        "agent",
                        360.0,
                        300.0,
                        agent("planner", true, "Plan the work and hand each step to the best agent."),
                    ),
                    node(
                        "n-agent-researcher",
                        "agent",
                        700.0,
                        180.0,
                        {
                            let mut a = agent("researcher", false, "Find facts on the web and cite them.");
                            a["description"] = json!("Looks things up on the web (http_request) and reports facts with sources. Not for writing files.");
                            a
                        },
                    ),
                    node(
                        "n-agent-writer",
                        "agent",
                        700.0,
                        440.0,
                        {
                            let mut a = agent("writer", false, "Write clear documents into the workspace.");
                            a["description"] = json!("Writes and edits documents in the team workspace. Not for web research.");
                            a
                        },
                    ),
                    node(
                        "n-tool-http",
                        "tool",
                        1040.0,
                        140.0,
                        json!({ "tool": "http_request", "restrict": false }),
                    ),
                    node(
                        "n-workspace",
                        "workspace",
                        1040.0,
                        460.0,
                        json!({ "path": ws, "confine": true }),
                    ),
                    node(
                        "n-secret-openrouter",
                        "secret",
                        60.0,
                        520.0,
                        json!({ "env": "OPENROUTER_API_KEY", "backend": "vault" }),
                    ),
                ],
                vec![
                    edge("e-plans", "n-orchestrator", "n-agent-planner"),
                    edge("e-del-researcher", "n-agent-planner", "n-agent-researcher"),
                    edge("e-del-writer", "n-agent-planner", "n-agent-writer"),
                    edge("e-uses-http", "n-agent-researcher", "n-tool-http"),
                    edge("e-works-in", "n-agent-writer", "n-workspace"),
                    edge("e-key-planner", "n-secret-openrouter", "n-agent-planner"),
                ],
            )
        }
        "telegram" => (
            vec![
                sb,
                node("n-telegram", "telegram", 60.0, 260.0, json!({ "allowed_users": [] })),
                node(
                    "n-agent-assistant",
                    "agent",
                    400.0,
                    260.0,
                    agent("assistant", false, "You are a helpful assistant on Telegram. Answer briefly."),
                ),
                node(
                    "n-secret-bot",
                    "secret",
                    60.0,
                    460.0,
                    json!({ "env": "TELEGRAM_BOT_TOKEN", "backend": "vault" }),
                ),
                node(
                    "n-secret-openrouter",
                    "secret",
                    60.0,
                    600.0,
                    json!({ "env": "OPENROUTER_API_KEY", "backend": "vault" }),
                ),
            ],
            vec![
                edge("e-chats", "n-telegram", "n-agent-assistant"),
                edge("e-bot-token", "n-secret-bot", "n-telegram"),
                edge("e-key", "n-secret-openrouter", "n-agent-assistant"),
            ],
        ),
        _ => return None,
    };
    Some(Blueprint {
        schema_version: BLUEPRINT_SCHEMA_VERSION,
        sandbox: sandbox.to_string(),
        nodes,
        edges,
        view: View::default(),
    })
}
