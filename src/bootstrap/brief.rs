//! The brief (`[agents.<a>.brief]`, rules in `domain/brief.rs`): before an
//! agent's turn its searcher, another agent, reads the request; what it found
//! goes in front of the request. Called by A2A agent turns
//! (`a2a::agent_turn`) and `run-agent` plan steps. It never fails the turn:
//! a searcher that errs or runs past its timeout is logged and the request
//! goes on as it was.

use std::time::{Duration, Instant};

use crate::config::{AgentConfig, Config};
use crate::domain::brief;

/// `request` with the brief from `agent`'s searcher in front; `request` as
/// it was when the agent has no brief, or its searcher found nothing, failed
/// or ran past `timeout_secs`. The searcher gets `request` alone, without
/// the conversation's history.
pub(crate) async fn with_brief(
    cfg: &Config,
    agent_name: &str,
    agent: &AgentConfig,
    request: &str,
) -> String {
    let Some(b) = &agent.brief else {
        return request.to_string();
    };
    let started = Instant::now();
    // Boxed: `agent_turn` calls this function, and this one `agent_turn`.
    // The searcher has no brief of its own (`Config::validate`), so the
    // recursion is one level deep.
    let turn = Box::pin(crate::bootstrap::a2a::agent_turn(
        cfg,
        &b.searcher,
        None,
        &[],
        request,
    ));
    let answer = tokio::time::timeout(Duration::from_secs(b.timeout_secs), turn).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match answer {
        Ok(Ok(answer)) => match brief::from_answer(&answer) {
            Some(found) => {
                tracing::info!(
                    agent = %agent_name,
                    searcher = %b.searcher,
                    brief_chars = found.chars().count(),
                    elapsed_ms,
                    "brief: the searcher's finding leads the request"
                );
                // The text itself only at debug (`RUST_LOG=tengu::bootstrap::brief=debug`):
                // it is the user's knowledge, redacted like an answer.
                if tracing::enabled!(tracing::Level::DEBUG) {
                    let secrets = crate::adapters::outbound::secrets::process_secret_registry(None);
                    tracing::debug!(
                        agent = %agent_name,
                        searcher = %b.searcher,
                        brief = %secrets.redact(&found),
                        "brief: the text"
                    );
                }
                brief::prepend(&b.searcher, &found, request)
            }
            None => {
                tracing::info!(
                    agent = %agent_name,
                    searcher = %b.searcher,
                    elapsed_ms,
                    "brief: the searcher found nothing; no brief"
                );
                request.to_string()
            }
        },
        Ok(Err(e)) => {
            tracing::warn!(
                agent = %agent_name,
                searcher = %b.searcher,
                elapsed_ms,
                error = %format!("{e:#}"),
                "brief: the searcher failed; the turn goes on without a brief"
            );
            request.to_string()
        }
        Err(_) => {
            tracing::warn!(
                agent = %agent_name,
                searcher = %b.searcher,
                timeout_secs = b.timeout_secs,
                "brief: the searcher ran past its timeout; the turn goes on without a brief"
            );
            request.to_string()
        }
    }
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

    /// `persona` gets its brief from `searcher`, whose model server is at
    /// `base_url`.
    fn briefed(base_url: &str, timeout_secs: u64) -> Config {
        load(&format!(
            r#"
[agents.persona]
engine = "local"
model = "mock"
default = true

[agents.persona.brief]
searcher = "searcher"
timeout_secs = {timeout_secs}

[agents.searcher]
engine = "local"
model = "mock"

[agents.searcher.local]
base_url = "{base_url}"

[agents.searcher.limits]
context_window = 16384
"#
        ))
    }

    #[tokio::test]
    async fn without_a_brief_the_request_is_as_it_was() {
        let cfg = briefed("http://127.0.0.1:9/v1", 1);
        let out = with_brief(&cfg, "searcher", &cfg.agents["searcher"], "the request").await;
        assert_eq!(out, "the request");
    }

    #[tokio::test]
    async fn a_searcher_past_its_timeout_leaves_the_request() {
        // Connections queue on this listener and are never answered.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", silent.local_addr().unwrap());
        let cfg = briefed(&url, 1);
        let started = Instant::now();
        let out = with_brief(&cfg, "persona", &cfg.agents["persona"], "the request").await;
        assert_eq!(out, "the request");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "cut at timeout_secs, took {:?}",
            started.elapsed()
        );
    }
}
