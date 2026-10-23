//! A2A server composition (`tengu a2a serve`, `inbound/a2a.rs`): the
//! `A2aRunner` that answers a message in this process, the service
//! (`application/a2a/`) and each served endpoint's card.
//!
//! | Endpoint | Card (`domain/a2a/card.rs`) | A turn runs |
//! |---|---|---|
//! | `/a2a` (`orchestrator = true`) | name / description from `[a2a.server]` (else `tengu <sandbox>`), one skill per routable agent | a one-shot orchestrator turn — planner, then `run-agent` steps — on session `a2a-<contextId>`; the context's earlier turns lead the message |
//! | `/a2a/agents/<name>` | the agent's `description` and `example_queries` | one in-process turn of the agent with its tools (as a webhook turn: its workspace or a temp dir, its scopes, Claude Code through the MCP bridge); earlier turns as chat history |
//!
//! The answer and an error's text are redacted with the process's secret
//! registry before they leave (the vault values, `*_TOKEN` … env values).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use crate::application::a2a::{A2aService, Limits};
use crate::config::a2a::A2aServerConfig;
use crate::config::Config;
use crate::domain::a2a::card::{self, CardSpec};
use crate::domain::a2a::model::AgentCard;
use crate::domain::secrets::SecretRegistry;
use crate::ports::a2a::{A2aRunner, A2aTarget, A2aTurn};

/// Longest earlier turn text handed back to the planner (chars).
const HISTORY_TURN_CHARS: usize = 4_000;

/// The served endpoints' cards.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cards {
    /// `/a2a` — the planner front door.
    pub planner: Option<AgentCard>,
    /// `/a2a/agents/<name>`.
    pub agents: BTreeMap<String, AgentCard>,
}

impl Cards {
    /// The card at the root (`/.well-known/agent-card.json`): the planner's,
    /// else the first agent's.
    pub(crate) fn root(&self) -> Option<(A2aTarget, &AgentCard)> {
        if let Some(c) = &self.planner {
            return Some((A2aTarget::Planner, c));
        }
        self.agents
            .iter()
            .next()
            .map(|(n, c)| (A2aTarget::Agent(n.clone()), c))
    }
}

/// The `[a2a.server]` section, or why `tengu a2a serve` cannot start.
pub(crate) fn server_config(config: &Config) -> Result<&A2aServerConfig> {
    config
        .a2a
        .as_ref()
        .and_then(|a| a.server.as_ref())
        .ok_or_else(|| {
            anyhow!(
                "no [a2a.server] in this sandbox config — add one (orchestrator = true and / or \
                 agents = [...], token_env = \"…\"); see docs/a2a-2026-10-10.md"
            )
        })
}

fn sandbox_name(config: &Config) -> String {
    config
        .agents
        .values()
        .next()
        .map(|a| a.sandbox.owner().to_string())
        .or_else(|| config.sandbox_name.clone())
        .unwrap_or_else(|| crate::config::sections::DEFAULT_SANDBOX.to_string())
}

/// Every served endpoint's card (module table).
pub(crate) fn cards(config: &Config) -> Result<Cards> {
    let server = server_config(config)?;
    let base = server.base_url();
    let bearer = server.token_env.is_some();
    let version = env!("CARGO_PKG_VERSION").to_string();
    let sandbox = sandbox_name(config);
    let mut routable: Vec<(&String, &crate::config::AgentConfig)> = config
        .agents
        .iter()
        .filter(|(_, a)| a.description.is_some())
        .collect();
    routable.sort_by_key(|(n, _)| *n);
    let skill = |name: &str, a: &crate::config::AgentConfig| {
        card::agent_skill(
            name,
            a.description.as_deref().unwrap_or_default(),
            &a.example_queries,
        )
    };
    let planner = server.orchestrator.then(|| {
        let names: Vec<&str> = routable.iter().map(|(n, _)| n.as_str()).collect();
        card::build(&CardSpec {
            name: server
                .name
                .clone()
                .unwrap_or_else(|| format!("tengu {sandbox}")),
            description: server.description.clone().unwrap_or_else(|| {
                format!(
                    "Tengu sandbox `{sandbox}`: a planner routes each message to one of its agents ({}) and answers with the result.",
                    names.join(", ")
                )
            }),
            version: version.clone(),
            endpoint_url: format!("{base}/a2a"),
            bearer,
            skills: routable.iter().map(|(n, a)| skill(n, a)).collect(),
        })
    });
    let mut agents = BTreeMap::new();
    for name in &server.agents {
        let a = config
            .agents
            .get(name)
            .ok_or_else(|| anyhow!("a2a.server.agents: no [agents.{name}]"))?;
        agents.insert(
            name.clone(),
            card::build(&CardSpec {
                name: name.clone(),
                description: a.description.clone().unwrap_or_default(),
                version: version.clone(),
                endpoint_url: format!("{base}/a2a/agents/{name}"),
                bearer,
                skills: vec![skill(name, a)],
            }),
        );
    }
    Ok(Cards { planner, agents })
}

/// The service of `tengu a2a serve` over `config`.
pub(crate) fn build_service(config: &Config, secrets: Arc<SecretRegistry>) -> Result<A2aService> {
    let server = server_config(config)?;
    let runner = Arc::new(TurnRunner {
        config: Arc::new(config.clone()),
        secrets,
    });
    Ok(A2aService::new(
        runner,
        Arc::new(crate::adapters::outbound::clock::SystemClock),
        Limits {
            max_tasks: server.max_tasks,
            max_running: server.max_running,
            context_turns: server.context_turns,
            run_timeout: Duration::from_secs(server.run_timeout_secs),
        },
    ))
}

/// Answers an inbound turn (module table).
struct TurnRunner {
    config: Arc<Config>,
    secrets: Arc<SecretRegistry>,
}

#[async_trait]
impl A2aRunner for TurnRunner {
    async fn run(&self, target: &A2aTarget, turn: A2aTurn) -> Result<String> {
        let out = match target {
            A2aTarget::Planner => self.planner_turn(&turn).await,
            A2aTarget::Agent(name) => {
                agent_turn(&self.config, name, &turn.history, &turn.text, &self.secrets).await
            }
        };
        out.map(|text| self.secrets.redact(&text))
            .map_err(|e| anyhow!(self.secrets.redact(&format!("{e:#}"))))
    }
}

impl TurnRunner {
    async fn planner_turn(&self, turn: &A2aTurn) -> Result<String> {
        // The planner calls its own engine (no tools); steps are
        // `run-agent` children.
        let llm = crate::bootstrap::orchestrator::planner_llm(&self.config)?
            .ok_or_else(|| anyhow!("the planner is not available (no [orchestrator])"))?;
        let session_id = format!("a2a-{}", turn.context_id);
        let orchestrator =
            crate::bootstrap::orchestrator::build_orchestrator(&self.config, llm, session_id, None)
                .ok_or_else(|| {
                    anyhow!("the planner is not available ([orchestrator] engine must be \"rag\")")
                })?;
        Ok(orchestrator.handle(planner_message(turn)).await)
    }
}

/// The planner's message: the context's earlier turns (each cut at
/// [`HISTORY_TURN_CHARS`]), then the new message.
fn planner_message(turn: &A2aTurn) -> String {
    if turn.history.is_empty() {
        return turn.text.clone();
    }
    let cut = |s: &str| -> String {
        let n = s.chars().count();
        if n <= HISTORY_TURN_CHARS {
            s.to_string()
        } else {
            let kept: String = s.chars().take(HISTORY_TURN_CHARS).collect();
            format!("{kept} … [{} more chars]", n - HISTORY_TURN_CHARS)
        }
    };
    let mut out = format!(
        "Earlier in this conversation (A2A context {}):\n",
        turn.context_id
    );
    for (q, a) in &turn.history {
        out.push_str(&format!(
            "\n[the other agent] {}\n[you] {}\n",
            cut(q),
            cut(a)
        ));
    }
    out.push_str(&format!("\nNew message:\n{}", turn.text));
    out
}

/// One in-process turn of `agent_name` (`bootstrap::turn::run_once`): its
/// engine, workspace (or a temp dir for the turn), tools, skills and scopes —
/// or, for the `[orchestrator]` agent, no tools. `history` = earlier
/// `(user, assistant)` pairs; tool output redacted with `secrets`.
async fn agent_turn(
    cfg: &Config,
    agent_name: &str,
    history: &[(String, String)],
    text: &str,
    secrets: &Arc<SecretRegistry>,
) -> Result<String> {
    let no_tools = cfg
        .orchestrator
        .as_ref()
        .is_some_and(|o| o.agent == agent_name);
    let opts = crate::bootstrap::turn::TurnOpts {
        system: None,
        history,
        no_tools,
        memory: false,
        temp_prefix: "tengu-a2a-",
    };
    let response = crate::bootstrap::turn::run_once(cfg, agent_name, text, secrets, opts).await?;
    Ok(response.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(toml: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml).unwrap();
        Config::load(&path).unwrap()
    }

    const CONFIG: &str = r#"
[orchestrator]
agent = "planner"

[agents.planner]
engine = "openrouter"
model = "x/y"
default = true

[agents.researcher]
engine = "openrouter"
model = "x/y"
description = "Finds things."
example_queries = ["what is the BTC price?"]

[agents.writer]
engine = "openrouter"
model = "x/y"
description = "Writes things."

[agents.signer]
engine = "openrouter"
model = "x/y"

[a2a.server]
token_env = "TENGU_A2A_TOKEN"
public_url = "https://tengu.example.com/"
orchestrator = true
agents = ["writer"]
"#;

    #[test]
    fn cards_name_their_endpoints_and_skills() {
        let cards = cards(&load(CONFIG)).unwrap();
        let p = cards.planner.as_ref().unwrap();
        assert_eq!(
            p.supported_interfaces[0].url,
            "https://tengu.example.com/a2a"
        );
        let skills: Vec<&str> = p.skills.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            skills,
            ["researcher", "writer"],
            "private agents are never listed"
        );
        assert_eq!(p.skills[0].examples, ["what is the BTC price?"]);
        assert_eq!(p.required_schemes(), ["bearer"]);
        let w = &cards.agents["writer"];
        assert_eq!(
            w.supported_interfaces[0].url,
            "https://tengu.example.com/a2a/agents/writer"
        );
        assert_eq!(w.description, "Writes things.");
        assert_eq!(cards.root().unwrap().0, A2aTarget::Planner);
        assert!(!cards.agents.contains_key("researcher"));
    }

    #[test]
    fn earlier_turns_lead_the_planner_message() {
        let turn = A2aTurn {
            task_id: "t".into(),
            context_id: "ctx-1".into(),
            text: "and now?".into(),
            history: vec![("first?".into(), "first answer".into())],
        };
        let m = planner_message(&turn);
        assert!(
            m.starts_with("Earlier in this conversation (A2A context ctx-1):"),
            "{m}"
        );
        assert!(
            m.contains("[the other agent] first?\n[you] first answer"),
            "{m}"
        );
        assert!(m.ends_with("New message:\nand now?"), "{m}");
        let fresh = A2aTurn {
            history: vec![],
            ..turn
        };
        assert_eq!(planner_message(&fresh), "and now?");
    }

    #[test]
    fn no_server_section_says_how_to_add_one() {
        let e = server_config(&Config::default()).unwrap_err().to_string();
        assert!(e.contains("no [a2a.server]"), "{e}");
    }
}
