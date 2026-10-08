//! Composition for `[decision_loops.<name>]`: Jev client + the loop agent's
//! tool executor (same allow-list, scopes and workspace a `run-agent`
//! subprocess of that agent gets; wrapped in `SanitizedToolExecutor` with the
//! caller's `SecretRegistry`, then `egress::AttributedExecutor`: egress
//! records name the loop's agent, the event's session and the call id) + the
//! agent workspace's observation store
//! (`open_observation_store`: `<workspace>/.tengu/observations.db`, source of
//! `state.world`, + the history recorder when `[recorder]` is on; fail-soft)
//! → `application::decision_loop::DecisionLoop`. [`agent_tool_executor`] also
//! builds each `[feeds]` tool feed's executor (`bootstrap/runtime.rs`).
//!
//! Replay — the backtest gate arm (`docs/xlab-2026-10-01.md` § 7):
//!
//! | Helper | Builds |
//! |---|---|
//! | [`build_replay_loop`] | the real loop on the caller's clock (a `SimClock`), audit to the run's `decisions.jsonl` (trigger `backtest`); no history (`history = 0`: a decision sees its event alone, so its request — the cache key — never depends on the events before it), no observation store, no escalator (`escalate = false`), an executor that refuses every call — a loop whose actions name a tool or that reads `world` is refused |
//! | [`cached_decision_engine`] | `CachedDecisionEngine` on `<state dir>/backtests/decision-cache.db` over `JevClient::from_env(model, timeout)`; offline = no client (a miss fails) |
//! | [`build_gate`] | `tengu backtest --gate <loop>`'s `Gate` (`application/backtest/gate.rs`): [`cached_decision_engine`] for the loop's model + timeout, and K replay loops over it, each on its own `SimClock` |

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use tracing::warn;

use crate::adapters::outbound::decision_cache::{CachedDecisionEngine, DECISION_CACHE_DB};
use crate::adapters::outbound::decisions::JevClient;
use crate::adapters::outbound::egress::{AttributedExecutor, CallSession};
use crate::adapters::outbound::noop::{NoopActivity, NoopRuntimeToolExecutor};
use crate::adapters::outbound::observations::open_observation_store;
use crate::adapters::outbound::secrets::SanitizedToolExecutor;
use crate::application::backtest::gate::{Gate, GateWorker};
use crate::application::decision_loop::{AuditLog, DecisionLoop};
use crate::config::decision_loop::DecisionLoopConfig;
use crate::config::{AgentConfig, Config};
use crate::domain::secrets::SecretRegistry;
use crate::domain::trace::RunIds;
use crate::ports::clock::{Clock, SimClock};
use crate::ports::decision::{DecisionEngine, Escalator};
use crate::ports::engine::ToolExecutor;

/// `trigger` of every replay audit line.
const REPLAY_TRIGGER: &str = "backtest";

/// `<TENGU_HOME>/logs/decisions.jsonl` — one line per decision.
pub(crate) fn audit_path() -> PathBuf {
    crate::config::paths::resolve_tengu_home()
        .join("logs")
        .join("decisions.jsonl")
}

/// Tool executor of `agent` for a decision loop or a `[feeds]` feed: the
/// allow-list, scopes and workspace a `run-agent` child of that agent gets,
/// wrapped in `SanitizedToolExecutor` with the process `secrets` — plus the
/// tool names it runs (none when no executor could be built: every call
/// then fails).
pub(crate) fn agent_tool_executor(
    config: &Config,
    agent: &AgentConfig,
    workspace: &Path,
    secrets: &Arc<SecretRegistry>,
) -> (Arc<dyn ToolExecutor>, BTreeSet<String>) {
    let (_, executor) = crate::bootstrap::tools::build_subprocess_tool_executor(
        agent,
        config,
        workspace,
        secrets,
        Arc::new(NoopActivity),
        None,
    );
    let (inner, runs): (Arc<dyn ToolExecutor>, BTreeSet<String>) = match executor {
        Some(e) => {
            let runs = e.additional_tool_defs(&[]).into_iter().map(|d| d.name);
            (Arc::new(e), runs.collect())
        }
        None => (
            Arc::new(crate::adapters::outbound::noop::NoopRuntimeToolExecutor),
            BTreeSet::new(),
        ),
    };
    let tools = Arc::new(SanitizedToolExecutor::new(inner, Arc::clone(secrets)));
    (tools, runs)
}

/// `secrets` is the process registry: tool text and typed observations are
/// redacted with it before they reach history, the audit log or Jev. `ids`:
/// the recording every audit line names (`bootstrap::trace::run_ids`;
/// default = none).
pub(crate) fn build_decision_loop(
    config: &Config,
    name: &str,
    escalator: Option<Arc<dyn Escalator>>,
    secrets: Arc<SecretRegistry>,
    ids: RunIds,
) -> Result<Arc<DecisionLoop>> {
    let dl = config
        .decision_loops
        .get(name)
        .ok_or_else(|| anyhow!("no [decision_loops.{name}] block in this config"))?;
    build_loop(config, name, dl.clone(), escalator, secrets, None, ids)
}

/// `[decision_loops.<name>]` narrowed by an execution map
/// (`config::execution_map`): `dl` = `ExecutionMap::apply` of that block —
/// same agent, tools, scopes and store; every audit line carries `trigger`
/// (`map:<sha256>`). No escalator (`tengu decide`).
pub(crate) fn build_mapped_loop(
    config: &Config,
    name: &str,
    dl: DecisionLoopConfig,
    trigger: String,
    secrets: Arc<SecretRegistry>,
    ids: RunIds,
) -> Result<Arc<DecisionLoop>> {
    build_loop(config, name, dl, None, secrets, Some(trigger), ids)
}

fn build_loop(
    config: &Config,
    name: &str,
    dl: DecisionLoopConfig,
    escalator: Option<Arc<dyn Escalator>>,
    secrets: Arc<SecretRegistry>,
    trigger: Option<String>,
    ids: RunIds,
) -> Result<Arc<DecisionLoop>> {
    let dl = &dl;
    let agent = config.agents.get(&dl.agent).ok_or_else(|| {
        anyhow!(
            "decision_loops.{name}.agent: no [agents.{}] block",
            dl.agent
        )
    })?;

    let engine = Arc::new(JevClient::from_env(
        &dl.model,
        Duration::from_secs(dl.timeout_secs),
    )?);

    let workspace = agent
        .workspace
        .as_ref()
        .map(|p| crate::config::paths::expand_tilde(p))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let (tools, _) = agent_tool_executor(config, agent, &workspace, &secrets);
    // Egress records name the loop's agent + the event's session + the call
    // id (every loop runs in one process: the env cannot).
    let tools: Arc<dyn ToolExecutor> = Arc::new(AttributedExecutor::new(
        tools,
        &dl.agent,
        CallSession::Loop(name.to_string()),
    ));

    let observations = match open_observation_store(&workspace, &agent.sandbox) {
        Ok(s) => Some(s),
        Err(e) => {
            let error = format!("{e:#}");
            warn!(decision_loop = %name, %error, "observation store unavailable; world reads as errors");
            None
        }
    };

    Ok(Arc::new(DecisionLoop::new(
        name,
        dl.clone(),
        engine,
        tools,
        observations,
        escalator,
        Some(AuditLog {
            path: audit_path(),
            sandbox: config.sandbox_name.clone(),
            trigger,
            ids,
        }),
    )))
}

/// `[decision_loops.<name>]` for replay: `engine` (normally
/// [`cached_decision_engine`]) answers, `clock` (a `SimClock` the caller
/// sets to each decision instant) is the loop's time, audit lines (trigger
/// `backtest`) go to `audit_path` — the backtest run's `decisions.jsonl`,
/// never `<TENGU_HOME>/logs/decisions.jsonl`. No history (`history = 0`:
/// every decision's state is `{goal, event, history: [], step: 0}`, so a
/// verdict and its cache key never depend on which candidates the loop
/// decided before — any worker count, any `--max-decisions`), no
/// observation store, no escalator (`escalate = false`), an executor that
/// refuses every call. Refused: a loop whose actions name a tool (replay
/// never runs tools) or that reads `world` (no store to read; the event
/// carries the features).
pub(crate) fn build_replay_loop(
    config: &Config,
    name: &str,
    engine: Arc<dyn DecisionEngine>,
    clock: Arc<dyn Clock>,
    audit_path: &Path,
) -> Result<Arc<DecisionLoop>> {
    let dl = config
        .decision_loops
        .get(name)
        .ok_or_else(|| anyhow!("no [decision_loops.{name}] block in this config"))?;
    let with_tool: Vec<String> = dl
        .actions
        .iter()
        .filter_map(|(an, a)| a.tool.as_ref().map(|t| format!("{an} (tool `{t}`)")))
        .collect();
    if !with_tool.is_empty() {
        bail!(
            "replay runs terminal-only loops: take / skip / ask_architect — \
             [decision_loops.{name}] actions {} name a tool",
            with_tool.join(", ")
        );
    }
    if !dl.world.is_empty() {
        bail!(
            "replay has no observation store: [decision_loops.{name}] must not read `world` \
             (the replayed event carries the features)"
        );
    }
    let mut cfg = dl.clone();
    cfg.escalate = false;
    cfg.history = 0;
    let audit = AuditLog {
        path: audit_path.to_path_buf(),
        sandbox: config.sandbox_name.clone(),
        trigger: Some(REPLAY_TRIGGER.to_string()),
        ids: Default::default(),
    };
    let tools: Arc<dyn ToolExecutor> = Arc::new(NoopRuntimeToolExecutor);
    Ok(Arc::new(
        DecisionLoop::new(name, cfg, engine, tools, None, None, Some(audit)).with_clock(clock),
    ))
}

/// The replay engine: `CachedDecisionEngine` on
/// `<state_dir>/backtests/decision-cache.db`; misses go to
/// `JevClient::from_env(model, timeout)` (needs `OPENROUTER_API_KEY`), or
/// fail when `offline` (no client, no key needed). Counters:
/// `DecisionEngine::cache_stats`.
pub(crate) fn cached_decision_engine(
    state_dir: &Path,
    model: &str,
    timeout: Duration,
    offline: bool,
) -> Result<Arc<dyn DecisionEngine>> {
    let inner: Option<Arc<dyn DecisionEngine>> = if offline {
        None
    } else {
        Some(Arc::new(JevClient::from_env(model, timeout)?))
    };
    let path = crate::config::xmarket::backtests_dir(state_dir).join(DECISION_CACHE_DB);
    Ok(Arc::new(CachedDecisionEngine::open(&path, model, inner)?))
}

/// The gate arm of `tengu backtest --gate <loop>`
/// (`application/backtest/gate.rs`): [`cached_decision_engine`] on
/// `state_dir` (the `[xmarket]` state dir) for `[decision_loops.<loop>]`'s
/// model and timeout — `offline` = no client, a miss fails — and
/// `concurrency` replay loops over it ([`build_replay_loop`]), each on its
/// own `SimClock`, every audit line to `audit_path` (the run's
/// `decisions.jsonl`). Refused: concurrency 0, an unknown loop, a loop that
/// is not terminal-only.
pub(crate) fn build_gate(
    config: &Config,
    loop_name: &str,
    state_dir: &Path,
    audit_path: &Path,
    concurrency: usize,
    offline: bool,
) -> Result<Gate> {
    if concurrency == 0 {
        bail!("jev gate: concurrency must be at least 1");
    }
    let dl = config
        .decision_loops
        .get(loop_name)
        .ok_or_else(|| anyhow!("no [decision_loops.{loop_name}] block in this config"))?;
    let engine = cached_decision_engine(
        state_dir,
        &dl.model,
        Duration::from_secs(dl.timeout_secs),
        offline,
    )?;
    let workers = (0..concurrency)
        .map(|_| {
            let clock = Arc::new(SimClock::at(0));
            let replay = build_replay_loop(
                config,
                loop_name,
                Arc::clone(&engine),
                clock.clone(),
                audit_path,
            )?;
            Ok((clock, replay))
        })
        .collect::<Result<Vec<GateWorker>>>()?;
    Ok(Gate {
        loop_name: loop_name.to_string(),
        engine,
        workers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::decision::{Answer, Decision, Question, StepOutcome};
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::collections::BTreeMap;

    const T0: i64 = 1_790_000_000_000;

    /// `xl_gate` (take / skip / ask_architect), `xl_watch` (a tool action),
    /// `xl_world` (reads `world`).
    fn config() -> Config {
        let mut c: Config = toml::from_str(
            r#"
            [agents.xl_jev]
            engine = "openrouter"
            model = "m"
            tools = ["http_request"]

            [decision_loops.xl_gate]
            goal = "Take a candidate only when its move looks like noise"
            agent = "xl_jev"
            act_at = 0.7
            [decision_loops.xl_gate.actions.take]
            description = "Trade this candidate"
            [decision_loops.xl_gate.actions.skip]
            description = "Do not trade it"
            [decision_loops.xl_gate.actions.ask_architect]
            description = "Unsure: hand it to the architect"

            [decision_loops.xl_watch]
            goal = "g"
            agent = "xl_jev"
            [decision_loops.xl_watch.actions.hold]
            description = "nothing"
            [decision_loops.xl_watch.actions.fetch]
            description = "fetch"
            tool = "http_request"
            read_only = true

            [decision_loops.xl_world]
            goal = "g"
            agent = "xl_jev"
            world = { price = "price_oracle/1:So11111111111111111111111111111111111111112" }
            [decision_loops.xl_world.actions.hold]
            description = "nothing"
            "#,
        )
        .unwrap();
        c.sandbox_name = Some("xlab".into());
        c
    }

    /// Always `take` at 0.9.
    struct Take;

    #[async_trait]
    impl DecisionEngine for Take {
        fn model(&self) -> &str {
            "~typesafe/jev-latest"
        }
        async fn decide(&self, _s: &Value, q: &BTreeMap<String, Question>) -> Result<Decision> {
            assert!(q.contains_key("next_action"), "{q:?}");
            let next = Answer {
                kind: "choice".into(),
                choice: Some("take".into()),
                probabilities: BTreeMap::from([("take".into(), 0.9), ("skip".into(), 0.1)]),
                confidence: Some(0.9),
                ..Default::default()
            };
            Ok(Decision {
                id: "gen-dec-1".into(),
                answers: BTreeMap::from([("next_action".into(), next)]),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn replay_loop_runs_on_the_clock_and_audits_to_the_run_dir() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("backtests/r1/decisions.jsonl");
        let clock = Arc::new(SimClock::at(T0));
        let l = build_replay_loop(&config(), "xl_gate", Arc::new(Take), clock, &audit).unwrap();
        let v = l
            .decide_terminal(
                &json!({"instrument": "hyperliquid:xyz:TSLA"}),
                "backtest:r1:0",
            )
            .await
            .unwrap();
        assert_eq!(
            v.outcome,
            StepOutcome::Stopped {
                action: "take".into()
            }
        );
        assert_eq!(
            (v.action.as_str(), v.confidence, v.below_act_at),
            ("take", 0.9, false)
        );
        let line: Value =
            serde_json::from_str(std::fs::read_to_string(&audit).unwrap().trim()).unwrap();
        assert_eq!(line["trigger"], json!("backtest"));
        assert_eq!(line["ts_ms"], json!(T0));
        assert_eq!(line["sandbox"], json!("xlab"));
        assert_eq!(line["loop"], json!("xl_gate"));
        assert_eq!(line["session_id"], json!("backtest:r1:0"));
    }

    #[test]
    fn replay_refuses_tool_loops_world_loops_and_unknown_loops() {
        let c = config();
        let clock: Arc<dyn Clock> = Arc::new(SimClock::at(T0));
        let err = |name: &str| {
            build_replay_loop(&c, name, Arc::new(Take), clock.clone(), Path::new("unused"))
                .err()
                .unwrap()
                .to_string()
        };
        let tool = err("xl_watch");
        assert!(
            tool.starts_with("replay runs terminal-only loops: take / skip / ask_architect"),
            "{tool}"
        );
        assert!(tool.contains("fetch (tool `http_request`)"), "{tool}");
        let world = err("xl_world");
        assert!(world.contains("must not read `world`"), "{world}");
        assert!(err("nope").contains("no [decision_loops.nope]"));
    }

    #[tokio::test]
    async fn offline_cached_engine_needs_no_key_and_a_miss_fails() {
        let dir = tempfile::tempdir().unwrap();
        let engine = cached_decision_engine(
            dir.path(),
            "~typesafe/jev-latest",
            Duration::from_secs(20),
            true,
        )
        .unwrap();
        assert!(dir.path().join("backtests/decision-cache.db").exists());
        assert_eq!(engine.model(), "~typesafe/jev-latest");
        let err = engine
            .decide(&json!({}), &BTreeMap::new())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("offline"), "{err:#}");
        assert_eq!(engine.cache_stats().map(|s| s.errors), Some(1));
    }

    /// Answers like [`Take`]; records every state it is asked.
    #[derive(Default)]
    struct Recording(std::sync::Mutex<Vec<Value>>);

    #[async_trait]
    impl DecisionEngine for Recording {
        fn model(&self) -> &str {
            "~typesafe/jev-latest"
        }
        async fn decide(&self, s: &Value, q: &BTreeMap<String, Question>) -> Result<Decision> {
            self.0.lock().unwrap().push(s.clone());
            Take.decide(s, q).await
        }
    }

    /// A replayed decision never sees an earlier one: its state (and so its
    /// cache key) is the event alone, whatever the config's `history`.
    #[tokio::test]
    async fn replay_loops_carry_no_history_between_events() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(Recording::default());
        let l = build_replay_loop(
            &config(),
            "xl_gate",
            engine.clone(),
            Arc::new(SimClock::at(T0)),
            &dir.path().join("decisions.jsonl"),
        )
        .unwrap();
        for (seq, id) in ["hyperliquid:xyz:TSLA", "hyperliquid:xyz:NVDA"]
            .iter()
            .enumerate()
        {
            let v = l
                .decide_terminal(&json!({"instrument": id}), &format!("backtest:r1:{seq}"))
                .await
                .unwrap();
            assert_eq!(v.action, "take");
        }
        assert!(l.history().await.is_empty());
        let states = engine.0.lock().unwrap();
        assert_eq!(states.len(), 2);
        for s in states.iter() {
            assert_eq!((&s["history"], &s["step"]), (&json!([]), &json!(0)), "{s}");
        }
        assert_eq!(
            states[1]["event"],
            json!({"instrument": "hyperliquid:xyz:NVDA"})
        );
    }

    #[test]
    fn build_gate_makes_k_workers_on_their_own_clocks() {
        let dir = tempfile::tempdir().unwrap();
        let (c, audit) = (config(), dir.path().join("run/decisions.jsonl"));
        let gate = build_gate(&c, "xl_gate", dir.path(), &audit, 3, true).unwrap();
        assert_eq!(gate.loop_name, "xl_gate");
        assert_eq!(gate.engine.model(), "~typesafe/jev-latest");
        assert_eq!(gate.workers.len(), 3);
        assert!(!Arc::ptr_eq(&gate.workers[0].0, &gate.workers[1].0));
        gate.workers[0].0.set(T0);
        assert_eq!(gate.workers[1].0.now_ms(), 0, "clocks are per worker");
        assert!(dir.path().join("backtests/decision-cache.db").exists());
        let err = |name: &str, k: usize| {
            build_gate(&c, name, dir.path(), &audit, k, true)
                .err()
                .unwrap()
                .to_string()
        };
        assert!(err("xl_gate", 0).contains("concurrency must be at least 1"));
        assert!(err("nope", 2).contains("no [decision_loops.nope]"));
        assert!(err("xl_watch", 2).contains("terminal-only"));
    }
}
