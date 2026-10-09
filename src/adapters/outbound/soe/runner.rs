//! `SubprocessStageRunner` — `ports::soe::StageRunner` as one `tengu
//! run-agent` child per model stage (`SubprocessRunner`, the plan steps'
//! machinery): the Architect and the Critic are agents of the sandbox, their
//! tools write the drafts into the run dir, the child returns how it went.
//!
//! | Rule | Value |
//! |---|---|
//! | Agent | `[agents.<req.agent>]` with a `description` (a `run-agent` step refuses one without) — else `Err` before any child |
//! | IPC | `AgentIpcInput`: `agent_name`, `goal` (ids and hashes only, `application/soe/cycle.rs`), `session_id` = the run id (a live cycle: its cycle id), `step_id` = the stage (`ARCHITECT` · `CHALLENGE`), `sandbox_config` = this sandbox, `max_turns` = the agent's `limits.max_tool_rounds`; model, tools and skills stay empty — the child takes them from its own `[agents.<name>]` block |
//! | Wall clock | the agent's `limits.step_timeout_secs` (it must cover the Architect's turn) |
//! | Reply ([`reply_of`]) | `Ok` ⇒ `ok`, `summary` = the child's summary (its output when that is empty); `Failed` ⇒ not ok, `error`, `summary` = the partial output; the metrics kept either way and re-emitted on the global sink (as `run_step`) |
//! | `Err` | no child, a non-zero exit, a timeout, an unparsable reply — the cycle records a failed stage and goes on |
//! | Skill identity ([`skill_sha256`]) | canonical sha256 of `{ "skills": { <name>: <sha256 of its SKILL.md> \| "MISSING" } }` over the agent's `skill_packages`, each found by `application::skills::registry::skill_directories` (first hit wins) — a part of the stage cache key (`cache.rs`) |

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Instant;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::json;

use crate::adapters::outbound::subprocess_runner::{
    AgentIpcInput, AgentIpcOutput, SubprocessRunner,
};
use crate::application::skills::registry::skill_directories;
use crate::config::AgentConfig;
use crate::domain::canonical::{canonical_sha256, sha256_hex};
use crate::ports::soe::{StageReply, StageRequest, StageRunner};

/// The `run-agent` stage runner of one sandbox (module table).
pub(crate) struct SubprocessStageRunner {
    runner: SubprocessRunner,
}

impl SubprocessStageRunner {
    /// Stages of `sandbox` (`--sandbox`; `None` = the default config) run
    /// as these agents.
    pub(crate) fn new(sandbox: Option<String>, agents: HashMap<String, AgentConfig>) -> Self {
        Self {
            runner: SubprocessRunner::new(sandbox, String::new(), agents),
        }
    }

    /// Spawn this binary instead of the current exe (tests).
    #[cfg(test)]
    fn with_exe(mut self, exe: std::path::PathBuf) -> Self {
        self.runner.tengu_path = Some(exe);
        self
    }

    /// The child's IPC payload for `req` (module table: IPC).
    fn input(&self, req: &StageRequest, agent: &AgentConfig) -> AgentIpcInput {
        AgentIpcInput {
            goal: req.goal.clone(),
            agent_name: req.agent.clone(),
            model: String::new(),
            tools: Vec::new(),
            skills: Vec::new(),
            max_turns: agent.limits.max_tool_rounds,
            sandbox: None,
            session_id: req.dir.id().to_string(),
            step_id: stage_id(req),
            compose: None,
            sandbox_config: self.runner.sandbox_name.clone(),
            plan_state: None,
        }
    }
}

/// `ARCHITECT` · `CHALLENGE`.
fn stage_id(req: &StageRequest) -> String {
    serde_json::to_value(req.stage)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{:?}", req.stage))
}

/// Module table: Reply.
pub(crate) fn reply_of(out: AgentIpcOutput, latency_ms: u64) -> StageReply {
    match out {
        AgentIpcOutput::Ok {
            output,
            summary,
            metrics,
            ..
        } => StageReply {
            ok: true,
            summary: if summary.trim().is_empty() {
                output
            } else {
                summary
            },
            error: None,
            latency_ms,
            metrics,
        },
        AgentIpcOutput::Failed {
            error,
            output,
            metrics,
            ..
        } => StageReply {
            ok: false,
            summary: output,
            error: Some(error),
            latency_ms,
            metrics,
        },
    }
}

#[async_trait]
impl StageRunner for SubprocessStageRunner {
    async fn run(&self, req: &StageRequest) -> Result<StageReply> {
        let agent = self
            .runner
            .agents
            .get(&req.agent)
            .filter(|a| {
                a.description
                    .as_deref()
                    .is_some_and(|d| !d.trim().is_empty())
            })
            .ok_or_else(|| {
                anyhow!(
                    "stage {}: no [agents.{}] block with a description — a stage runs as a \
                     `run-agent` step",
                    stage_id(req),
                    req.agent
                )
            })?;
        let input = self.input(req, agent);
        let started = Instant::now();
        let out = self
            .runner
            .run_with_timeout(input, agent.limits.step_timeout_secs)
            .await?;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let reply = reply_of(out, latency_ms);
        for rec in &reply.metrics {
            crate::application::metrics::forward(rec.clone());
        }
        Ok(reply)
    }
}

/// Module table: Skill identity, over `workspace`'s skill directories.
pub(crate) fn skill_sha256(agent: &AgentConfig, workspace: &Path) -> String {
    let dirs = skill_directories(workspace);
    let mut skills = BTreeMap::new();
    for name in &agent.skill_packages {
        let found = dirs
            .iter()
            .map(|d| d.join(name).join("SKILL.md"))
            .find(|p| p.is_file())
            .and_then(|p| std::fs::read(p).ok());
        let sha = match found {
            Some(bytes) => sha256_hex(&String::from_utf8_lossy(&bytes)),
            None => "MISSING".to_string(),
        };
        skills.insert(name.clone(), sha);
    }
    canonical_sha256(&json!({ "skills": skills }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::metrics::{MetricsKind, MetricsRecord};
    use crate::domain::soe::ops::Stage;
    use crate::ports::soe::RunDir;

    fn metric(agent: &str) -> MetricsRecord {
        MetricsRecord {
            ts_unix: 0,
            session_id: "2026-W41".into(),
            kind: MetricsKind::Subagent,
            agent: agent.into(),
            model: "synthetic-model".into(),
            prompt_tokens: 12_000,
            completion_tokens: 900,
            total_tokens: 12_900,
            prompt_chars: 0,
            prompt_bytes: 0,
            response_chars: 0,
            latency_ms: 1_000,
            layers: Vec::new(),
            step_id: Some("ARCHITECT".into()),
        }
    }

    fn agents() -> HashMap<String, AgentConfig> {
        let cfg: crate::config::Config = toml::from_str(
            r#"
            [agents.soe_architect]
            engine = "openrouter"
            model = "m"
            description = "Proposes mechanisms"
            tools = ["soe_propose"]
            [agents.soe_architect.limits]
            step_timeout_secs = 30

            [agents.nameless]
            engine = "openrouter"
            model = "m"
            tools = ["read_file"]
            "#,
        )
        .unwrap();
        cfg.agents
    }

    fn request(agent: &str) -> StageRequest {
        StageRequest {
            stage: Stage::Architect,
            agent: agent.into(),
            dir: RunDir::Cycle("2026-W41".into()),
            cycle_id: "2026-W41".into(),
            goal: "soe_stage: ARCHITECT\nrun: cycles/2026-W41\n".into(),
        }
    }

    /// A fake `tengu` that drains stdin, saves it beside itself and prints
    /// `reply` (an `AgentIpcOutput` as JSON).
    #[cfg(unix)]
    fn fake_child(dir: &Path, reply: &AgentIpcOutput) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let out = dir.join("reply.json");
        std::fs::write(&out, serde_json::to_vec(reply).unwrap()).unwrap();
        let exe = dir.join("fake-tengu");
        std::fs::write(
            &exe,
            format!(
                "#!/bin/sh\ncat > '{}'\ncat '{}'\n",
                dir.join("stdin.json").display(),
                out.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        exe
    }

    /// A child that fails keeps its metrics (tokens were spent), its error
    /// and its partial output; the IPC names the run, the stage and the
    /// sandbox. An agent without a description never spawns a child.
    #[cfg(unix)]
    #[tokio::test]
    async fn failed_child_keeps_metrics() {
        let tmp = tempfile::TempDir::new().unwrap();
        let failed = AgentIpcOutput::Failed {
            error: "model finished without calling compress_and_store".into(),
            output: "2 proposals written".into(),
            metrics: vec![metric("soe_architect"), metric("soe_architect")],
            tools: Vec::new(),
        };
        let runner = SubprocessStageRunner::new(Some("soe".into()), agents())
            .with_exe(fake_child(tmp.path(), &failed));
        let reply = runner.run(&request("soe_architect")).await.unwrap();
        assert!(!reply.ok);
        assert_eq!(
            reply.error.as_deref(),
            Some("model finished without calling compress_and_store")
        );
        assert_eq!(reply.summary, "2 proposals written");
        assert_eq!(reply.metrics.len(), 2);
        assert_eq!(
            reply.metrics.iter().map(|m| m.prompt_tokens).sum::<u32>(),
            24_000
        );
        let sent: AgentIpcInput =
            serde_json::from_slice(&std::fs::read(tmp.path().join("stdin.json")).unwrap()).unwrap();
        assert_eq!(
            (
                sent.agent_name.as_str(),
                sent.session_id.as_str(),
                sent.step_id.as_str(),
                sent.sandbox_config.as_deref(),
            ),
            ("soe_architect", "2026-W41", "ARCHITECT", Some("soe"))
        );
        assert_eq!(sent.goal, request("soe_architect").goal);
        assert!(sent.tools.is_empty() && sent.model.is_empty());

        // An ok child: the summary, else its output.
        let ok = AgentIpcOutput::Ok {
            output: "done".into(),
            summary: String::new(),
            metrics: vec![metric("soe_architect")],
            tools: Vec::new(),
        };
        let r = reply_of(ok, 7);
        assert!(r.ok && r.error.is_none());
        assert_eq!(
            (r.summary.as_str(), r.latency_ms, r.metrics.len()),
            ("done", 7, 1)
        );

        for agent in ["nameless", "absent"] {
            let tmp2 = tempfile::TempDir::new().unwrap();
            let runner = SubprocessStageRunner::new(Some("soe".into()), agents())
                .with_exe(fake_child(tmp2.path(), &failed));
            let e = runner.run(&request(agent)).await.unwrap_err().to_string();
            assert!(e.contains("with a description"), "{e}");
            assert!(
                !tmp2.path().join("stdin.json").exists(),
                "no child for {agent}"
            );
        }
    }

    /// The skill identity changes with a SKILL.md; a missing skill is named.
    #[test]
    fn skill_identity_follows_the_skill_files() {
        let ws = tempfile::TempDir::new().unwrap();
        let mut agent = agents().remove("soe_architect").unwrap();
        agent.skill_packages = vec!["soe-architect-test-skill".into()];
        let missing = skill_sha256(&agent, ws.path());
        let dir = ws.path().join(".tengu/skills/soe-architect-test-skill");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "# v1\n").unwrap();
        let v1 = skill_sha256(&agent, ws.path());
        std::fs::write(dir.join("SKILL.md"), "# v2\n").unwrap();
        let v2 = skill_sha256(&agent, ws.path());
        assert_eq!(v1.len(), 64);
        assert!(missing != v1 && v1 != v2, "{missing} {v1} {v2}");
        agent.skill_packages.clear();
        assert_eq!(
            skill_sha256(&agent, ws.path()),
            canonical_sha256(&json!({ "skills": {} }))
        );
    }
}
