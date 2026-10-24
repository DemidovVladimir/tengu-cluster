//! Tests of `src/application/decision_loop/mod.rs`.

use super::*;
use crate::domain::decision::Answer;
use crate::domain::message::Message;
use async_trait::async_trait;
use std::sync::Mutex as StdMutex;

/// Scripted decision engine: returns queued decisions, records questions
/// and states.
struct Scripted {
    queue: StdMutex<VecDeque<Decision>>,
    seen: StdMutex<Vec<BTreeMap<String, Question>>>,
    states: StdMutex<Vec<Value>>,
}

impl Scripted {
    fn new(script: Vec<Decision>) -> Self {
        Self {
            queue: StdMutex::new(script.into()),
            seen: StdMutex::new(vec![]),
            states: StdMutex::new(vec![]),
        }
    }
}

#[async_trait]
impl DecisionEngine for Scripted {
    fn model(&self) -> &str {
        "test"
    }
    async fn decide(&self, s: &Value, q: &BTreeMap<String, Question>) -> Result<Decision> {
        self.seen.lock().unwrap().push(q.clone());
        self.states.lock().unwrap().push(s.clone());
        Ok(self
            .queue
            .lock()
            .unwrap()
            .pop_front()
            .expect("no scripted decision"))
    }
}

struct FakeTools(StdMutex<Vec<ToolCall>>);

#[async_trait]
impl ToolExecutor for FakeTools {
    async fn execute(&self, call: &ToolCall, _m: &[Message]) -> Result<String> {
        self.0.lock().unwrap().push(call.clone());
        Ok("HTTP 200 https://x\n{\"pools\":[{\"address\":\"A1\",\"fees\":9},{\"address\":\"B2\",\"fees\":1}]}".into())
    }
}

struct FakeEscalator(StdMutex<Vec<String>>);

#[async_trait]
impl Escalator for FakeEscalator {
    async fn escalate(&self, _s: String, m: String) {
        self.0.lock().unwrap().push(m);
    }
}

fn pick(pairs: &[(&str, &str, f64)]) -> Decision {
    Decision {
        answers: pairs
            .iter()
            .map(|(k, c, conf)| {
                (
                    k.to_string(),
                    Answer {
                        kind: "choice".into(),
                        choice: Some(c.to_string()),
                        confidence: Some(*conf),
                        ..Default::default()
                    },
                )
            })
            .collect(),
        ..Default::default()
    }
}

fn cfg(dry_run: bool) -> DecisionLoopConfig {
    toml::from_str(&format!(
            r#"
goal = "open best pool, max 2"
agent = "a"
dry_run = {dry_run}
[actions.hold]
description = "nothing"
[actions.fetch]
description = "fetch pools"
tool = "http_request"
read_only = true
args = {{ method = "GET", url = "https://x/pools" }}
reduce = {{ pools = "/pools/*/{{address,fees}}" }}
[actions.open]
description = "open position"
tool = "open_position"
args = {{ pool = "{{pool}}", size = "{{size}}" }}
slots = {{ pool = {{ from = "fetch", items = "/pools/*", value = "address" }}, size = [0.5, 1, 2, 3] }}
caps = {{ size = 2.0 }}
"#
        ))
        .unwrap()
}

fn build(
    dry_run: bool,
    script: Vec<Decision>,
) -> (
    DecisionLoop,
    Arc<Scripted>,
    Arc<FakeTools>,
    Arc<FakeEscalator>,
) {
    let engine = Arc::new(Scripted::new(script));
    let tools = Arc::new(FakeTools(StdMutex::new(vec![])));
    let esc = Arc::new(FakeEscalator(StdMutex::new(vec![])));
    let l = DecisionLoop::new(
        "t",
        cfg(dry_run),
        engine.clone(),
        tools.clone(),
        None,
        Some(esc.clone()),
        None,
    );
    (l, engine, tools, esc)
}

// ── tool-call ids ─────────────────────────────────────────────────

fn fetch_then_hold() -> Vec<Decision> {
    vec![
        pick(&[("next_action", "fetch", 0.95)]),
        pick(&[("next_action", "hold", 0.99)]),
    ]
}

/// `{loop}:{session_id}:{t}`: two sessions never share an id, and a
/// restarted loop (`t` from 0 again) never repeats an earlier one.
#[tokio::test]
async fn call_ids_carry_the_session_and_survive_restarts() {
    let (l, _, tools, _) = build(false, [fetch_then_hold(), fetch_then_hold()].concat());
    l.handle_event(&json!({}), "webhook-a").await.unwrap();
    l.handle_event(&json!({}), "webhook-b").await.unwrap();
    let (restarted, _, after, _) = build(false, fetch_then_hold());
    restarted
        .handle_event(&json!({}), "webhook-c")
        .await
        .unwrap();
    let ids: Vec<String> = tools
        .0
        .lock()
        .unwrap()
        .iter()
        .chain(after.0.lock().unwrap().iter())
        .map(|c| c.id.clone())
        .collect();
    assert_eq!(ids, ["t:webhook-a:1", "t:webhook-b:3", "t:webhook-c:1"]);
}

/// Through the real executor the id reaches the tool as `ToolCtx.call_id`.
#[tokio::test]
async fn loop_call_id_reaches_the_tool_ctx() {
    use crate::application::tools::registry::{PluginToolExecutor, ToolRegistry};
    use crate::domain::message::ToolDef;
    use crate::ports::tool::{Tool, ToolCtx};

    struct Recorder {
        def: ToolDef,
        seen: Arc<StdMutex<Vec<Option<String>>>>,
    }
    #[async_trait]
    impl Tool for Recorder {
        fn definition(&self) -> &ToolDef {
            &self.def
        }
        async fn execute(&self, _args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
            // scope: pure-compute
            self.seen
                .lock()
                .unwrap()
                .push(ctx.call_id.map(str::to_string));
            Ok(ToolOutput::from(
                "HTTP 200 https://x\n{\"pools\":[]}".to_string(),
            ))
        }
    }
    struct Quiet;
    impl crate::ports::tool_activity::ToolActivityPort for Quiet {
        fn publish_tool_activity(&self, _call: &ToolCall) {}
    }

    let seen = Arc::new(StdMutex::new(vec![]));
    let mut registry = ToolRegistry::new();
    registry.register_tool(Arc::new(Recorder {
        def: ToolDef::new("http_request", "d", json!({})),
        seen: Arc::clone(&seen),
    }));
    let exec = PluginToolExecutor {
        registry,
        workspace: PathBuf::from("."),
        shell: Arc::new(crate::adapters::outbound::shell::LocalShellExecutor::new()),
        http: reqwest::Client::new(),
        secret_registry: Arc::new(crate::domain::secrets::SecretRegistry::new()),
        activity: Arc::new(Quiet),
        scopes: Default::default(),
        agent_config: None,
    };
    let l = DecisionLoop::new(
        "exec",
        cfg(false),
        Arc::new(Scripted::new(fetch_then_hold())),
        Arc::new(exec),
        None,
        None,
        None,
    );
    l.handle_event(&json!({}), "decide-exec-7").await.unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        [Some("exec:decide-exec-7:1".to_string())]
    );
}

// ── typed tools, world, requires, FromObservation ─────────────────

use crate::application::observe::tests::MemStore;
use crate::domain::observation::{ErrorClass, ObsSource, ObsStatus, Observation, ReadError};
use crate::ports::tool::ToolOutput;

const MINT: &str = "So11111111111111111111111111111111111111112";
const POOL_A: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";
const POOL_B: &str = "BGm1tav58oGcsQJehL9WXBFXF7D27vZsKefj4xJKD5Y";
const POOLS_KEY: &str = "dlmm_pools/1:SOL-USDC|fee_tvl_24h|10|100000";

fn price_obs(observed_at_ms: i64, status: ObsStatus) -> Observation {
    Observation {
        key: format!("price_oracle/1:{MINT}"),
        schema: "price_oracle/1".into(),
        tool: "sol_price".into(),
        observed_at_ms,
        slot: None,
        ttl_ms: 10_000,
        source: ObsSource::Live,
        status,
        errors: if status == ObsStatus::Error {
            vec![ReadError::new(
                "usd",
                ErrorClass::Timeout,
                "no source answered",
            )]
        } else {
            vec![]
        },
        headline: format!("sol_price {MINT} usd=150.25"),
        features: [("usd".to_string(), json!(150.25))].into(),
        data: json!({"mint": MINT, "usd": 150.25}),
    }
}

fn pools_obs(observed_at_ms: i64) -> Observation {
    Observation {
        key: POOLS_KEY.into(),
        schema: "dlmm_pools/1".into(),
        tool: "dlmm_pools".into(),
        observed_at_ms,
        slot: None,
        ttl_ms: 60_000,
        source: ObsSource::Live,
        status: ObsStatus::Ok,
        errors: vec![],
        headline: "dlmm_pools SOL-USDC n=2".into(),
        features: [("n_pools".to_string(), json!(2))].into(),
        data: json!({"pools": [{"address": POOL_A, "tvl_usd": 7.0e6}, {"address": POOL_B, "tvl_usd": 1.0e6}]}),
    }
}

/// Typed executor: every call returns a price observation with `status`.
struct TypedTools {
    calls: StdMutex<Vec<ToolCall>>,
    status: ObsStatus,
}

#[async_trait]
impl ToolExecutor for TypedTools {
    async fn execute(&self, call: &ToolCall, m: &[Message]) -> Result<String> {
        Ok(self.execute_typed(call, m).await?.text)
    }
    async fn execute_typed(&self, call: &ToolCall, _m: &[Message]) -> Result<ToolOutput> {
        self.calls.lock().unwrap().push(call.clone());
        let now = now_ms();
        Ok(ToolOutput::observed(price_obs(now, self.status), now))
    }
}

fn typed_cfg() -> DecisionLoopConfig {
    toml::from_str(&format!(
        r#"
goal = "keep the LP safe"
agent = "a"
dry_run = false
world = {{ price = "price_oracle/1:{MINT}", pools = "{POOLS_KEY}" }}
[actions.hold]
description = "nothing"
[actions.refresh]
description = "refresh price"
tool = "sol_price"
read_only = true
args = {{ mint = "{MINT}" }}
[actions.refresh_reduced]
description = "refresh price, reduced"
tool = "sol_price"
read_only = true
reduce = {{ usd = "/features/usd", mint = "/data/mint" }}
[actions.open]
description = "open position"
tool = "dlmm_open_position"
requires = {{ price = 30 }}
args = {{ pool = "{{pool}}" }}
slots = {{ pool = {{ observation = "pools", items = "/data/pools/*", value = "address" }} }}
"#
    ))
    .unwrap()
}

fn build_typed(
    script: Vec<Decision>,
    status: ObsStatus,
    store: Option<Arc<MemStore>>,
) -> (DecisionLoop, Arc<Scripted>, Arc<TypedTools>) {
    let engine = Arc::new(Scripted::new(script));
    let tools = Arc::new(TypedTools {
        calls: StdMutex::new(vec![]),
        status,
    });
    let store = store.map(|s| s as Arc<dyn ObservationStore>);
    let l = DecisionLoop::new(
        "t",
        typed_cfg(),
        engine.clone(),
        tools.clone(),
        store,
        None,
        None,
    );
    (l, engine, tools)
}

fn legal_actions(engine: &Scripted, step: usize) -> Vec<String> {
    let seen = engine.seen.lock().unwrap();
    let Question::Choice { criteria, .. } = &seen[step][NEXT_ACTION] else {
        panic!()
    };
    criteria.keys().cloned().collect()
}

#[tokio::test]
async fn typed_result_lands_in_history_with_obs_meta() {
    let (l, _, tools) = build_typed(
        vec![
            pick(&[("next_action", "refresh", 0.95)]),
            pick(&[("next_action", "refresh_reduced", 0.95)]),
            pick(&[("next_action", "hold", 0.99)]),
        ],
        ObsStatus::Ok,
        None,
    );
    l.handle_event(&json!({}), "s").await.unwrap();
    assert_eq!(
        tools.calls.lock().unwrap()[0].arguments,
        json!({"mint": MINT})
    );
    let st = l.state.lock().await;
    let h = &st.history[0];
    assert_eq!(h.ok, Some(true));
    assert_eq!(h.result["features"]["usd"], json!(150.25));
    assert_eq!(h.result["status"], json!("ok"));
    assert!(h.result.get("data").is_none(), "unreduced = features only");
    let meta = h.obs.as_ref().expect("typed result carries obs meta");
    assert_eq!(meta.key, format!("price_oracle/1:{MINT}"));
    assert_eq!(meta.source, ObsSource::Live);
    // The reducer addresses `{.., features, data}`.
    let r = &st.history[1];
    assert_eq!(r.result, json!({"usd": 150.25, "mint": MINT}));
    assert!(st.history[2].obs.is_none(), "terminal action has no obs");
}

#[tokio::test]
async fn typed_error_observation_is_not_ok() {
    let (l, _, _) = build_typed(
        vec![
            pick(&[("next_action", "refresh", 0.95)]),
            pick(&[("next_action", "hold", 0.99)]),
        ],
        ObsStatus::Error,
        None,
    );
    l.handle_event(&json!({}), "s").await.unwrap();
    let st = l.state.lock().await;
    assert_eq!(st.history[0].ok, Some(false));
    assert_eq!(st.history[0].obs.as_ref().unwrap().status, ObsStatus::Error);
    assert_eq!(st.history[0].result["errors"][0]["class"], json!("timeout"));
}

#[tokio::test]
async fn requires_hides_action_while_world_is_stale() {
    let store = Arc::new(MemStore::default());
    store
        .put(&price_obs(now_ms() - 60_000, ObsStatus::Ok))
        .await
        .unwrap();
    store.put(&pools_obs(now_ms())).await.unwrap();
    let (l, engine, _) = build_typed(
        vec![pick(&[("next_action", "hold", 0.99)])],
        ObsStatus::Ok,
        Some(store),
    );
    l.handle_event(&json!({}), "s").await.unwrap();
    assert!(!legal_actions(&engine, 0).contains(&"open".to_string()));
    let states = engine.states.lock().unwrap();
    let world = &states[0]["world"];
    assert_eq!(world["price"]["status"], json!("stale"));
    assert!(
        !world["price"].to_string().contains("150.25"),
        "stale entry carries no numbers: {world}"
    );
    assert_eq!(world["pools"]["features"]["n_pools"], json!(2));
}

#[tokio::test]
async fn observation_slot_offers_fresh_world_items() {
    let store = Arc::new(MemStore::default());
    store
        .put(&price_obs(now_ms(), ObsStatus::Ok))
        .await
        .unwrap();
    store.put(&pools_obs(now_ms())).await.unwrap();
    let (l, engine, tools) = build_typed(
        vec![
            pick(&[("next_action", "open", 0.95), ("open__pool", "pool_2", 0.9)]),
            pick(&[("next_action", "hold", 0.99)]),
        ],
        ObsStatus::Ok,
        Some(store),
    );
    l.handle_event(&json!({}), "s").await.unwrap();
    assert!(legal_actions(&engine, 0).contains(&"open".to_string()));
    let calls = tools.calls.lock().unwrap();
    assert_eq!(calls[0].name, "dlmm_open_position");
    assert_eq!(calls[0].arguments, json!({"pool": POOL_B}), "full address");
}

#[tokio::test]
async fn world_is_omitted_without_aliases() {
    let (l, engine, _, _) = build(false, vec![pick(&[("next_action", "hold", 0.99)])]);
    l.handle_event(&json!({}), "s").await.unwrap();
    assert!(engine.states.lock().unwrap()[0].get("world").is_none());
}

#[tokio::test]
async fn fetch_then_open_with_history_slots() {
    let (l, engine, tools, _) = build(
        false,
        vec![
            pick(&[("next_action", "fetch", 0.95)]),
            pick(&[
                ("next_action", "open", 0.9),
                ("open__pool", "pool_1", 0.93),
                ("open__size", "2", 0.97),
            ]),
            pick(&[("next_action", "hold", 0.99)]),
        ],
    );
    let out = l.handle_event(&json!({"sig": "x"}), "s").await.unwrap();
    assert_eq!(
        out,
        vec![
            StepOutcome::Executed {
                action: "fetch".into()
            },
            StepOutcome::Executed {
                action: "open".into()
            },
            StepOutcome::Stopped {
                action: "hold".into()
            },
        ]
    );
    // Step 1: `open` not legal yet (no fetch result) → not offered.
    let seen = engine.seen.lock().unwrap();
    let Question::Choice { criteria, .. } = &seen[0][NEXT_ACTION] else {
        panic!()
    };
    assert!(!criteria.contains_key("open"));
    // Step 2: pool candidates come from the fetch result; size capped at 2.
    let Question::Choice { criteria, .. } = &seen[1]["open__size"] else {
        panic!()
    };
    assert!(!criteria.contains_key("3"));
    let calls = tools.0.lock().unwrap();
    assert_eq!(calls[1].name, "open_position");
    assert_eq!(calls[1].arguments, json!({"pool": "A1", "size": 2}));
}

#[tokio::test]
async fn history_slots_never_reuse_an_earlier_events_result() {
    let (l, engine, tools, _) = build(
        false,
        vec![
            // Event 1: fetch succeeds, then hold.
            pick(&[("next_action", "fetch", 0.95)]),
            pick(&[("next_action", "hold", 0.99)]),
            // Event 2: no fetch yet — the event-1 pools must not feed `open`.
            pick(&[
                ("next_action", "open", 0.99),
                ("open__pool", "pool_1", 0.99),
                ("open__size", "1", 0.99),
            ]),
        ],
    );
    l.handle_event(&json!({"n": 1}), "s").await.unwrap();
    assert!(legal_actions(&engine, 1).contains(&"open".to_string()));
    let out = l.handle_event(&json!({"n": 2}), "s").await.unwrap();
    assert!(!legal_actions(&engine, 2).contains(&"open".to_string()));
    assert!(
        matches!(&out[0], StepOutcome::Rejected { action, .. } if action == "open"),
        "{out:?}"
    );
    assert_eq!(tools.0.lock().unwrap().len(), 1, "only event 1's fetch ran");
    // The earlier event's entries stay in `state.history` as context.
    let states = engine.states.lock().unwrap();
    assert_eq!(states[2]["history"][0]["action"], json!("fetch"));
}

#[tokio::test]
async fn dry_run_blocks_write_actions_and_goes_on() {
    let (l, _, tools, _) = build(
        true,
        vec![
            pick(&[("next_action", "fetch", 0.95)]),
            pick(&[
                ("next_action", "open", 0.9),
                ("open__pool", "pool_2", 0.9),
                ("open__size", "1", 0.9),
            ]),
            pick(&[("next_action", "hold", 0.99)]),
        ],
    );
    let out = l.handle_event(&json!({}), "s").await.unwrap();
    assert_eq!(
        out[1..],
        [
            StepOutcome::DryRun {
                action: "open".into()
            },
            StepOutcome::Stopped {
                action: "hold".into()
            }
        ]
    );
    assert_eq!(
        tools.0.lock().unwrap().len(),
        1,
        "only the read-only fetch ran"
    );
}

/// A chain bound end to end: the event (what a higher-order agent hands
/// the loop) fills `wallet` / `size`, the earlier `fetch` fills `pool` —
/// no slot question reaches Jev, and `open` is illegal until every
/// binding resolves.
fn chain(dry_run: bool, script: Vec<Decision>) -> (DecisionLoop, Arc<Scripted>, Arc<FakeTools>) {
    let cfg: DecisionLoopConfig = toml::from_str(&format!(
            r#"
goal = "fetch, then open what the event asks for"
agent = "a"
dry_run = {dry_run}
[actions.hold]
description = "nothing"
[actions.fetch]
description = "fetch pools"
tool = "http_request"
read_only = true
args = {{ method = "GET", url = "https://x/pools" }}
[actions.open]
description = "open position"
tool = "open_position"
args = {{ wallet = "{{wallet}}", pool = "{{pool}}", size = "{{size}}", mode = "simulate" }}
slots = {{ wallet = {{ event = "/wallet" }}, size = {{ event = "/size" }}, pool = {{ from = "fetch", path = "/pools/0/address" }} }}
caps = {{ size = 2.0 }}
"#
        ))
        .unwrap();
    assert!(cfg.validation_errors("chain").is_empty());
    let engine = Arc::new(Scripted::new(script));
    let tools = Arc::new(FakeTools(StdMutex::new(vec![])));
    let l = DecisionLoop::new(
        "chain",
        cfg,
        engine.clone(),
        tools.clone(),
        None,
        None,
        None,
    );
    (l, engine, tools)
}

fn fetch_open_hold() -> Vec<Decision> {
    vec![
        pick(&[("next_action", "fetch", 0.95)]),
        pick(&[("next_action", "open", 0.95)]),
        pick(&[("next_action", "hold", 0.99)]),
    ]
}

#[tokio::test]
async fn bound_slots_fill_args_from_event_and_history() {
    let (l, engine, tools) = chain(false, fetch_open_hold());
    let out = l
        .handle_event(&json!({"wallet": "W1", "size": 1.5}), "s")
        .await
        .unwrap();
    assert_eq!(out.len(), 3, "{out:?}");
    assert!(!legal_actions(&engine, 0).contains(&"open".to_string()));
    assert!(legal_actions(&engine, 1).contains(&"open".to_string()));
    let seen = engine.seen.lock().unwrap();
    assert!(
        seen[1].keys().all(|k| !k.starts_with("open__")),
        "bound slots ask nothing: {:?}",
        seen[1].keys()
    );
    let calls = tools.0.lock().unwrap();
    assert_eq!(
        calls[1].arguments,
        json!({"wallet": "W1", "pool": "A1", "size": 1.5, "mode": "simulate"})
    );
}

#[tokio::test]
async fn unresolved_or_capped_bindings_keep_the_action_illegal() {
    for event in [
        json!({"wallet": "W1", "size": 3}), // above the cap
        json!({"size": 1}),                 // no wallet
        json!({"wallet": null, "size": 1}), // null is nothing
    ] {
        let (l, engine, tools) = chain(
            false,
            vec![
                pick(&[("next_action", "fetch", 0.95)]),
                pick(&[("next_action", "hold", 0.99)]),
            ],
        );
        l.handle_event(&event, "s").await.unwrap();
        assert!(
            !legal_actions(&engine, 1).contains(&"open".to_string()),
            "{event}"
        );
        assert_eq!(tools.0.lock().unwrap().len(), 1, "{event}");
    }
}

#[tokio::test]
async fn dry_run_walks_the_chain_without_its_write() {
    let (l, _, tools) = chain(true, fetch_open_hold());
    let out = l
        .handle_event(&json!({"wallet": "W1", "size": 1}), "s")
        .await
        .unwrap();
    assert_eq!(
        out,
        [
            StepOutcome::Executed {
                action: "fetch".into()
            },
            StepOutcome::DryRun {
                action: "open".into()
            },
            StepOutcome::Stopped {
                action: "hold".into()
            },
        ]
    );
    assert_eq!(tools.0.lock().unwrap().len(), 1, "the write never ran");
    let h = l.history().await;
    assert_eq!(h[1].args["pool"], json!("A1"), "dry-run args are resolved");
}

// ── sequence ──────────────────────────────────────────────────────

/// `broken` answers HTTP 500 (a failed step); everything else 200.
struct SeqTools(StdMutex<Vec<ToolCall>>);

#[async_trait]
impl ToolExecutor for SeqTools {
    async fn execute(&self, call: &ToolCall, _m: &[Message]) -> Result<String> {
        self.0.lock().unwrap().push(call.clone());
        Ok(if call.name == "broken" {
            "HTTP 500 https://x\nboom".into()
        } else {
            "HTTP 200 https://x\n{\"ok\":1}".into()
        })
    }
}

fn sequenced(
    sequence: &str,
    script: Vec<Decision>,
) -> (DecisionLoop, Arc<Scripted>, Arc<SeqTools>) {
    let cfg: DecisionLoopConfig = toml::from_str(&format!(
        r#"
goal = "g"
agent = "a"
max_steps = 5
dry_run = false
sequence = {sequence}
[actions.hold]
description = "nothing"
[actions.a]
description = "a"
tool = "http_request"
args = {{ url = "https://x/a" }}
[actions.b]
description = "b"
tool = "http_request"
args = {{ url = "https://x/b" }}
[actions.maybe]
description = "needs the event's x"
tool = "t"
slots = {{ x = {{ event = "/x" }} }}
[actions.broken]
description = "fails"
tool = "broken"
"#
    ))
    .unwrap();
    assert!(
        cfg.validation_errors("s").is_empty(),
        "{:?}",
        cfg.validation_errors("s")
    );
    let engine = Arc::new(Scripted::new(script));
    let tools = Arc::new(SeqTools(StdMutex::new(vec![])));
    let l = DecisionLoop::new("s", cfg, engine.clone(), tools.clone(), None, None, None);
    (l, engine, tools)
}

#[tokio::test]
async fn sequence_offers_only_the_next_step_and_skips_unresolved_optionals() {
    let (l, engine, tools) = sequenced(
        r#"["b", "maybe?", "a"]"#,
        vec![
            pick(&[("next_action", "b", 0.95)]),
            pick(&[("next_action", "a", 0.95)]),
            pick(&[("next_action", "hold", 0.99)]),
        ],
    );
    l.handle_event(&json!({}), "s").await.unwrap();
    assert_eq!(legal_actions(&engine, 0), ["b", "hold"]);
    assert_eq!(
        legal_actions(&engine, 1),
        ["a", "hold"],
        "maybe? skipped: no event x"
    );
    assert_eq!(legal_actions(&engine, 2), ["hold"], "done: terminals only");
    let urls: Vec<_> = tools
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.arguments["url"].clone())
        .collect();
    assert_eq!(urls, [json!("https://x/b"), json!("https://x/a")]);
}

#[tokio::test]
async fn sequence_runs_a_resolved_optional_step() {
    let (l, engine, _) = sequenced(
        r#"["maybe?", "a"]"#,
        vec![
            pick(&[("next_action", "maybe", 0.95)]),
            pick(&[("next_action", "a", 0.95)]),
            pick(&[("next_action", "hold", 0.99)]),
        ],
    );
    l.handle_event(&json!({"x": 1}), "s").await.unwrap();
    assert_eq!(legal_actions(&engine, 0), ["hold", "maybe"]);
    assert_eq!(legal_actions(&engine, 1), ["a", "hold"]);
}

#[tokio::test]
async fn a_failed_step_halts_the_chain() {
    let (l, engine, tools) = sequenced(
        r#"["broken", "a"]"#,
        vec![
            pick(&[("next_action", "broken", 0.95)]),
            pick(&[("next_action", "hold", 0.99)]),
        ],
    );
    let out = l.handle_event(&json!({}), "s").await.unwrap();
    assert_eq!(
        legal_actions(&engine, 1),
        ["hold"],
        "a is never offered after a failure"
    );
    assert!(matches!(out.last(), Some(StepOutcome::Stopped { .. })));
    assert_eq!(tools.0.lock().unwrap().len(), 1);
    // The next event starts the chain again.
    let (l2, engine2, _) = sequenced(
        r#"["broken", "a"]"#,
        vec![
            pick(&[("next_action", "broken", 0.95)]),
            pick(&[("next_action", "hold", 0.99)]),
            pick(&[("next_action", "hold", 0.99)]),
        ],
    );
    l2.handle_event(&json!({}), "s").await.unwrap();
    l2.handle_event(&json!({}), "s").await.unwrap();
    assert_eq!(legal_actions(&engine2, 2), ["broken", "hold"]);
}

#[tokio::test]
async fn an_unresolved_required_step_offers_terminals_only() {
    let (l, engine, tools) = sequenced(
        r#"["maybe", "a"]"#,
        vec![pick(&[("next_action", "hold", 0.99)])],
    );
    l.handle_event(&json!({}), "s").await.unwrap();
    assert_eq!(legal_actions(&engine, 0), ["hold"]);
    assert!(tools.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_dry_run_step_moves_the_chain_on() {
    let (l, engine, tools) = sequenced(
        r#"["b", "a"]"#,
        vec![
            pick(&[("next_action", "b", 0.95)]),
            pick(&[("next_action", "a", 0.95)]),
            pick(&[("next_action", "hold", 0.99)]),
        ],
    );
    let mut cfg = l.cfg.clone();
    cfg.dry_run = true;
    let l = DecisionLoop::new("s", cfg, engine.clone(), tools.clone(), None, None, None);
    let out = l.handle_event(&json!({}), "s").await.unwrap();
    assert_eq!(legal_actions(&engine, 1), ["a", "hold"]);
    assert!(matches!(&out[1], StepOutcome::DryRun { action } if action == "a"));
    assert!(tools.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_off_sequence_answer_is_rejected() {
    let (l, _, tools) = sequenced(r#"["b", "a"]"#, vec![pick(&[("next_action", "a", 0.95)])]);
    let out = l.handle_event(&json!({}), "s").await.unwrap();
    assert!(
        matches!(&out[0], StepOutcome::Rejected { action, .. } if action == "a"),
        "{out:?}"
    );
    assert!(tools.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn low_slot_confidence_escalates() {
    let (l, _, tools, esc) = build(
        false,
        vec![
            pick(&[("next_action", "fetch", 0.95)]),
            pick(&[
                ("next_action", "open", 0.9),
                ("open__pool", "pool_1", 0.4),
                ("open__size", "1", 0.9),
            ]),
        ],
    );
    let out = l.handle_event(&json!({}), "s").await.unwrap();
    assert!(
        matches!(out.last(), Some(StepOutcome::Escalated { confidence, .. }) if *confidence == 0.4)
    );
    assert_eq!(tools.0.lock().unwrap().len(), 1);
    let msgs = esc.0.lock().unwrap();
    assert!(
        msgs[0].contains("\"A1\""),
        "escalation shows the full resolved value"
    );
}

// ── audit ──────────────────────────────────────────────────────────

/// The decisions endpoint failing (402, timeout, 5xx).
struct Failing;

#[async_trait]
impl DecisionEngine for Failing {
    fn model(&self) -> &str {
        "~typesafe/jev-latest"
    }
    async fn decide(&self, _s: &Value, _q: &BTreeMap<String, Question>) -> Result<Decision> {
        Err(anyhow::anyhow!("decisions HTTP 402: insufficient credits"))
    }
}

fn audited(name: &str, engine: Arc<dyn DecisionEngine>, path: &Path) -> DecisionLoop {
    DecisionLoop::new(
        name,
        cfg(false),
        engine,
        Arc::new(FakeTools(StdMutex::new(vec![]))),
        None,
        None,
        Some(AuditLog {
            path: path.to_path_buf(),
            sandbox: Some("xmarket".into()),
            trigger: None,
            ids: Default::default(),
        }),
    )
}

/// 4 writers × 500 lines into one file (loops of one process; other
/// processes append the same way): every line parses whole.
#[test]
fn concurrent_audit_lines_never_interleave() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs").join("decisions.jsonl");
    let loops: Vec<DecisionLoop> = (0..4)
        .map(|i| audited(&format!("loop{i}"), Arc::new(Scripted::new(vec![])), &path))
        .collect();
    let d = pick(&[
        ("next_action", "open", 0.91),
        ("open__pool", "pool_2", 0.88),
        ("open__size", "2", 0.97),
    ]);
    let entry = HistoryEntry {
        t: 1,
        action: "open".into(),
        args: json!({"pool": POOL_B, "size": 2}),
        ok: Some(true),
        result: json!({"pools": (0..40).map(|i| json!({"address": POOL_A, "fees": i})).collect::<Vec<_>>()}),
        obs: None,
    };
    let outcome = StepOutcome::Executed {
        action: "open".into(),
    };
    let (d, entry, outcome) = (&d, &entry, &outcome);
    std::thread::scope(|s| {
        for l in &loops {
            s.spawn(move || {
                for t in 0..500 {
                    l.audit("s", t, Some(d), outcome, Some(entry), 7);
                }
            });
        }
    });
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 2000);
    let mut per_loop: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for line in text.lines() {
        let v: Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("torn audit line ({e}): {line}"));
        assert_eq!(v["output"]["pools"].as_array().unwrap().len(), 40);
        per_loop
            .entry(v["loop"].as_str().unwrap().to_string())
            .or_default()
            .push(v["t"].as_u64().unwrap());
    }
    assert_eq!(per_loop.len(), 4);
    for (name, mut ts) in per_loop {
        ts.sort_unstable();
        assert_eq!(ts, (0..500).collect::<Vec<u64>>(), "{name}");
    }
}

/// `runtime_id` / `run_id` join a line only when the `AuditLog` names
/// them (additive keys: a line without them keeps its old shape).
#[tokio::test]
async fn audit_lines_carry_runtime_and_run_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("decisions.jsonl");
    let runtime_id = "host:4242:0f0e0d0c-0b0a-4908-8706-050403020100";
    let run_id = "5b0c7d0e-8a4e-4f0a-9d8e-2f1c3b4a5d6e";
    let mut traced = audited(
        "t",
        Arc::new(Scripted::new(vec![pick(&[("next_action", "hold", 0.99)])])),
        &path,
    );
    if let Some(a) = traced.audit.as_mut() {
        a.ids = crate::domain::trace::RunIds {
            runtime_id: Some(runtime_id.into()),
            run_id: Some(run_id.into()),
        };
    }
    traced.handle_event(&json!({}), "tick:1").await.unwrap();
    let plain = Arc::new(Scripted::new(vec![pick(&[("next_action", "hold", 0.99)])]));
    audited("t", plain, &path)
        .handle_event(&json!({}), "s2")
        .await
        .unwrap();
    let lines: Vec<Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["runtime_id"], json!(runtime_id));
    assert_eq!(lines[0]["run_id"], json!(run_id));
    assert_eq!(lines[0]["session_id"], json!("tick:1"));
    assert!(lines[1].get("runtime_id").is_none() && lines[1].get("run_id").is_none());
    assert!(render_audit(&lines[0]).is_some());
    assert_eq!(
        render_audit(&lines[0]),
        render_audit(&lines[1]),
        "the TUI feed renders both alike"
    );
}

/// Old keys stay, ms fields join; a failed decisions call leaves one
/// `error` line, then its error propagates; the feed renders it.
#[tokio::test]
async fn audit_lines_for_decided_and_failed_calls() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("decisions.jsonl");
    let hold = Arc::new(Scripted::new(vec![pick(&[("next_action", "hold", 0.99)])]));
    audited("t", hold, &path)
        .handle_event(&json!({}), "s1")
        .await
        .unwrap();
    let err = audited("t", Arc::new(Failing), &path)
        .handle_event(&json!({}), "s2")
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("402"), "{err:#}");

    let lines: Vec<Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    let (ok, failed) = (&lines[0], &lines[1]);
    for key in [
        "ts",
        "ts_ms",
        "loop",
        "sandbox",
        "session_id",
        "t",
        "call_id",
        "decision_id",
        "model",
        "act_at",
        "latency_ms",
        "answers",
        "usage",
        "result",
        "args",
        "ok",
        "output",
        "obs",
    ] {
        assert!(ok.get(key).is_some(), "{key}: {ok}");
        assert!(failed.get(key).is_some(), "{key}: {failed}");
    }
    assert_eq!(
        ok["result"],
        json!({"outcome": "stopped", "action": "hold"})
    );
    assert!(ok["call_id"].is_null(), "a terminal step ran no tool");
    assert_eq!(ok["sandbox"], json!("xmarket"));
    assert_eq!(ok["act_at"], json!(0.8));
    assert!(ok["ts_ms"].as_u64().unwrap() >= ok["ts"].as_u64().unwrap() * 1000);
    assert!(ok["latency_ms"].is_u64());

    assert_eq!(
        failed["result"],
        json!({"outcome": "error", "reason": "decisions HTTP 402: insufficient credits"})
    );
    assert_eq!(failed["session_id"], json!("s2"));
    assert!(failed["answers"].is_null() && failed["decision_id"].is_null());
    assert_eq!(failed["model"], json!("~typesafe/jev-latest"));
    assert_eq!(
        render_audit(failed).unwrap(),
        "jev t #0 · decide failed → error: decisions HTTP 402: insufficient credits"
    );
}

/// A failed decisions call is a `decision` metrics record too: `ok` false,
/// no usage, the engine's model.
#[tokio::test]
async fn a_failed_decisions_call_is_a_failed_metrics_record() {
    use tokio::sync::broadcast::error::TryRecvError;
    let mut rx = crate::application::metrics::install_global_sink().subscribe();
    let session = format!("jev-fail-{}", uuid::Uuid::new_v4());
    let dir = tempfile::tempdir().unwrap();
    audited("t", Arc::new(Failing), &dir.path().join("d.jsonl"))
        .handle_event(&json!({}), &session)
        .await
        .unwrap_err();
    // Other tests share the process-global sink: skip their records.
    let rec = loop {
        match rx.try_recv() {
            Ok(r) if r.session_id == session => break Some(r),
            Ok(_) | Err(TryRecvError::Lagged(_)) => continue,
            Err(_) => break None,
        }
    }
    .expect("a record for the failed call");
    assert!(!rec.ok);
    assert_eq!(
        (rec.kind, rec.total_tokens, rec.model.as_str()),
        (MetricsKind::Decision, 0, "~typesafe/jev-latest")
    );
}

// ── risk refusals + the verdict join ──────────────────────────────

const TSLA: &str = "hyperliquid:xyz:TSLA";
const OPP: &str = "xm_compare/1:hyperliquid:xyz:TSLA:hyperliquid:xyz:TSLA";

/// `enter_small` ($20) and `enter_big` ($30) run `paper_order`; `done`
/// stops.
fn entry_cfg() -> DecisionLoopConfig {
    let args = |usd: u32| {
        format!(
            "{{ instrument = \"{TSLA}\", side = \"buy\", notional_usd = {usd}, kind = \
                 \"market\", max_slippage_bps = 30, opportunity = \"{OPP}\" }}"
        )
    };
    toml::from_str(&format!(
        r#"
goal = "enter TSLA within the budget"
agent = "exec"
dry_run = false
[actions.done]
description = "stop"
[actions.enter_small]
description = "buy $20 of TSLA"
tool = "paper_order"
args = {}
[actions.enter_big]
description = "buy $30 of TSLA"
tool = "paper_order"
args = {}
"#,
        args(20),
        args(30)
    ))
    .unwrap()
}

/// Every call returns the `paper_fill/1` row of an order the gate denied
/// `min_edge`.
struct DenyingTools;

#[async_trait]
impl ToolExecutor for DenyingTools {
    async fn execute(&self, call: &ToolCall, m: &[Message]) -> Result<String> {
        Ok(self.execute_typed(call, m).await?.text)
    }
    async fn execute_typed(&self, call: &ToolCall, _m: &[Message]) -> Result<ToolOutput> {
        let now = now_ms();
        let features = [
            ("risk", "deny"),
            ("risk_rule", "min_edge"),
            ("status", "denied"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), json!(v)))
        .collect();
        let obs = Observation {
            key: format!("paper_fill/1:xmarket:{}", call.id),
            schema: "paper_fill/1".into(),
            tool: "paper_order".into(),
            observed_at_ms: now,
            slot: None,
            ttl_ms: 0,
            source: ObsSource::Live,
            status: ObsStatus::Error,
            errors: vec![ReadError::new(
                "risk",
                ErrorClass::NotApplicable,
                "denied min_edge: edge 4 bps < 10 bps",
            )],
            headline: format!("paper_fill denied buy {TSLA} risk=deny rule=min_edge"),
            features,
            data: Value::Null,
        };
        Ok(ToolOutput::observed(obs, now))
    }
}

/// A gate denial is `Refused` (counted apart from `Executed`), the loop
/// goes on; the audit line names the rule and the call; the feed shows it.
#[tokio::test]
async fn a_gate_denial_is_refused_and_the_loop_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("decisions.jsonl");
    let l = DecisionLoop::new(
        "entry",
        entry_cfg(),
        Arc::new(Scripted::new(vec![
            pick(&[("next_action", "enter_small", 0.95)]),
            pick(&[("next_action", "done", 0.99)]),
        ])),
        Arc::new(DenyingTools),
        None,
        None,
        Some(AuditLog {
            path: path.clone(),
            sandbox: None,
            trigger: None,
            ids: Default::default(),
        }),
    );
    let out = l.handle_event(&json!({}), "s").await.unwrap();
    assert_eq!(
        out,
        vec![
            StepOutcome::Refused {
                action: "enter_small".into(),
                rule: "min_edge".into()
            },
            StepOutcome::Stopped {
                action: "done".into()
            },
        ]
    );
    assert_eq!(l.history().await[0].ok, Some(false));
    let line: Value = serde_json::from_str(
        std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        line["result"],
        json!({"outcome": "refused", "action": "enter_small", "rule": "min_edge"})
    );
    assert_eq!(line["call_id"], json!("entry:s:1"));
    let text = render_audit(&line).unwrap();
    assert!(
        text.starts_with("jev entry #1 · enter_small (0.95) → refused by the risk gate: min_edge"),
        "{text}"
    );
    assert!(text.contains("\n  call   entry:s:1"), "{text}");
}

/// Runs `paper_order` through the real `run_exec` on an xm test rig
/// (scripted clock + book, ledger mirroring to `risk.jsonl`), keyed by
/// the loop's call id.
struct RigTools(crate::adapters::outbound::tools::xm::exec_common::tests::Rig);

#[async_trait]
impl ToolExecutor for RigTools {
    async fn execute(&self, call: &ToolCall, m: &[Message]) -> Result<String> {
        Ok(self.execute_typed(call, m).await?.text)
    }
    async fn execute_typed(&self, call: &ToolCall, _m: &[Message]) -> Result<ToolOutput> {
        let limits = self.0.shared.risk.as_ref().unwrap().limits();
        let order =
            crate::adapters::outbound::tools::xm::paper::parse_order(&call.arguments, limits)?;
        let obs = self.0.run(order, &call.id).await?;
        Ok(ToolOutput::observed(obs, now_ms()))
    }
}

/// Tracker convention 9: the decision audit line of a step that placed
/// an order and the order's risk verdict (`risk.jsonl`, the ledger row)
/// share the call id `{loop}:{session}:{t}` — allowed and denied alike.
#[tokio::test]
async fn loop_audit_and_risk_verdicts_join_by_call_id() {
    use crate::adapters::outbound::tools::xm::exec_common::tests::Rig;
    use crate::ports::paper::PaperLedger;

    let rig = Rig::new(25).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("decisions.jsonl");
    let tools = Arc::new(RigTools(rig));
    let l = DecisionLoop::new(
        "entry",
        entry_cfg(),
        Arc::new(Scripted::new(vec![
            pick(&[("next_action", "enter_small", 0.95)]),
            pick(&[("next_action", "enter_big", 0.95)]),
            pick(&[("next_action", "done", 0.99)]),
        ])),
        tools.clone(),
        None,
        None,
        Some(AuditLog {
            path: path.clone(),
            sandbox: Some("xmarket".into()),
            trigger: None,
            ids: Default::default(),
        }),
    );
    let out = l
        .handle_event(&json!({}), "0001318605-26-000123")
        .await
        .unwrap();
    assert_eq!(
        out[..2],
        [
            StepOutcome::Executed {
                action: "enter_small".into()
            },
            StepOutcome::Refused {
                action: "enter_big".into(),
                rule: "order_notional".into()
            },
        ]
    );
    let audit: Vec<Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let risk = tools.0.risk_lines();
    assert_eq!(risk.len(), 2, "one verdict per order");
    let joined: Vec<(String, String, String)> = audit
        .iter()
        .filter_map(|a| {
            let call = a["call_id"].as_str()?;
            let r = risk.iter().find(|r| r["call_id"] == call)?;
            Some((
                call.to_string(),
                a["result"]["outcome"].as_str()?.to_string(),
                r["verdict"].as_str()?.to_string(),
            ))
        })
        .collect();
    assert_eq!(
        joined,
        [
            (
                "entry:0001318605-26-000123:1".to_string(),
                "executed".to_string(),
                "allow".to_string()
            ),
            (
                "entry:0001318605-26-000123:2".to_string(),
                "refused".to_string(),
                "deny".to_string()
            ),
        ]
    );
    // The ledger rows carry the same keys.
    let rows = tools.0.ledger.decisions("xmarket", 10).await.unwrap();
    let mut calls: Vec<&str> = rows.iter().filter_map(|d| d.call_id.as_deref()).collect();
    calls.sort_unstable();
    assert_eq!(
        calls,
        [
            "entry:0001318605-26-000123:1",
            "entry:0001318605-26-000123:2"
        ]
    );
}

#[tokio::test]
async fn illegal_choice_is_rejected() {
    let (l, _, tools, _) = build(false, vec![pick(&[("next_action", "open", 0.99)])]);
    let out = l.handle_event(&json!({}), "s").await.unwrap();
    assert!(matches!(&out[0], StepOutcome::Rejected { action, .. } if action == "open"));
    assert!(tools.0.lock().unwrap().is_empty());
}

#[test]
fn render_audit_shows_action_slots_args_and_output() {
    let url = "https://api.coinbase.com/v2/prices/SOL-USD/spot";
    let line = json!({
        "loop": "executor", "t": 1,
        "answers": {
            "next_action": {"choice": "crypto_price", "confidence": 1.0},
            "crypto_price__pair": {"choice": "SOL-USD", "confidence": 0.97},
            "fx_rate__currency": {"choice": "EUR", "confidence": 0.9}
        },
        "result": {"action": "crypto_price", "outcome": "executed"},
        "args": {"method": "GET", "url": url},
        "ok": true,
        "output": {"price": {"amount": "118.29"}}
    });
    let text = render_audit(&line).unwrap();
    assert!(text.starts_with("jev executor #1 · crypto_price (1.00) → executed"));
    assert!(text.contains("pair = SOL-USD (0.97)"));
    assert!(!text.contains("EUR"), "other actions' slots are hidden");
    assert!(text.contains(url), "args are shown in full");
    assert!(text.contains("ok     {\"price\":{\"amount\":\"118.29\"}}"));
}

#[test]
fn render_audit_terminal_and_rejected() {
    let done = json!({
        "loop": "executor", "t": 2,
        "answers": {"next_action": {"choice": "done", "confidence": 1.0}},
        "result": {"action": "done", "outcome": "stopped"},
        "args": null, "ok": null, "output": null
    });
    assert_eq!(
        render_audit(&done).unwrap(),
        "jev executor #2 · done (1.00) → stopped"
    );
    let rej = json!({
        "loop": "l", "t": 0,
        "answers": {"next_action": {"choice": "open", "confidence": 0.99}},
        "result": {"action": "open", "outcome": "rejected", "reason": "not a legal action this step"}
    });
    assert!(render_audit(&rej)
        .unwrap()
        .ends_with("rejected: not a legal action this step"));
    assert!(render_audit(&json!({"loop": "l"})).is_none());
}

// ── clock + replay ────────────────────────────────────────────────

use crate::ports::clock::SimClock;

/// 2026-09-21 — ten days before these tests were written: on the wall
/// clock every row observed then is long stale.
const T0: i64 = 1_790_000_000_000;

/// `world` ages follow the loop's clock, not the wall: rows observed at
/// `T0` are fresh at `T0 + 20 s` and stale at `T0 + 40 s`
/// (`world_max_age_secs` 30; `open` requires `price` ≤ 30 s old).
#[tokio::test]
async fn world_freshness_follows_the_loop_clock() {
    let store = Arc::new(MemStore::default());
    store.put(&price_obs(T0, ObsStatus::Ok)).await.unwrap();
    store.put(&pools_obs(T0)).await.unwrap();
    let clock = Arc::new(SimClock::at(T0 + 20_000));
    let (l, engine, _) = build_typed(
        vec![
            pick(&[("next_action", "hold", 0.99)]),
            pick(&[("next_action", "hold", 0.99)]),
        ],
        ObsStatus::Ok,
        Some(store),
    );
    let l = l.with_clock(clock.clone());
    l.handle_event(&json!({}), "s").await.unwrap();
    clock.set(T0 + 40_000);
    l.handle_event(&json!({}), "s").await.unwrap();

    assert!(legal_actions(&engine, 0).contains(&"open".to_string()));
    assert!(!legal_actions(&engine, 1).contains(&"open".to_string()));
    let states = engine.states.lock().unwrap();
    assert_eq!(states[0]["world"]["price"]["age_s"], json!(20.0));
    assert_eq!(
        states[0]["world"]["price"]["features"]["usd"],
        json!(150.25)
    );
    assert_eq!(
        states[1]["world"]["price"],
        json!({"status": "stale", "age_s": 40.0})
    );
}

/// `ts` / `ts_ms` are the clock's; `trigger` only when the `AuditLog`
/// sets one.
#[tokio::test]
async fn audit_lines_carry_the_clock_time_and_the_trigger() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("decisions.jsonl");
    let clock = Arc::new(SimClock::at(T0 + 20_500));
    let engine = Arc::new(Scripted::new(vec![
        pick(&[("next_action", "hold", 0.99)]),
        pick(&[("next_action", "hold", 0.99)]),
    ]));
    let replayed = DecisionLoop::new(
        "t",
        cfg(false),
        engine.clone(),
        Arc::new(FakeTools(StdMutex::new(vec![]))),
        None,
        None,
        Some(AuditLog {
            path: path.clone(),
            sandbox: Some("xlab".into()),
            trigger: Some("backtest".into()),
            ids: Default::default(),
        }),
    )
    .with_clock(clock);
    replayed.handle_event(&json!({}), "s1").await.unwrap();
    let before = now_ms();
    audited("t", engine, &path)
        .handle_event(&json!({}), "s2")
        .await
        .unwrap();
    let after = now_ms();
    let lines: Vec<Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["ts_ms"], json!(T0 + 20_500));
    assert_eq!(lines[0]["ts"], json!((T0 + 20_500) / 1000));
    assert_eq!(lines[0]["trigger"], json!("backtest"));
    assert_eq!(lines[0]["sandbox"], json!("xlab"));
    assert!(lines[0]["latency_ms"].is_u64(), "latency stays real");
    // No clock: the wall; no trigger: no key.
    let wall = lines[1]["ts_ms"].as_i64().unwrap();
    assert!((before..=after).contains(&wall), "{wall}");
    assert!(lines[1].get("trigger").is_none(), "{}", lines[1]);
}

fn gate_cfg() -> DecisionLoopConfig {
    let mut c: DecisionLoopConfig = toml::from_str(
        r#"
goal = "Take a weekend-fade candidate only when its move looks like noise"
agent = "xl_jev"
act_at = 0.7
[actions.take]
description = "Trade this candidate"
[actions.skip]
description = "Do not trade it"
[actions.ask_architect]
description = "Unsure: hand it to the architect"
"#,
    )
    .unwrap();
    c.escalate = false;
    c
}

fn gate_pick(choice: &str, confidence: f64, p: [(&str, f64); 3]) -> Decision {
    Decision {
        id: format!("gen-dec-{choice}"),
        answers: BTreeMap::from([(
            NEXT_ACTION.to_string(),
            Answer {
                kind: "choice".into(),
                choice: Some(choice.into()),
                probabilities: p.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
                confidence: Some(confidence),
                ..Default::default()
            },
        )]),
        ..Default::default()
    }
}

/// The gate arm's view of one candidate: a confident pick is `Stopped`
/// with its confidence and p per action; one below `act_at` is
/// `Escalated` (no history entry). One decision per event, every action
/// offered, the event reaches the engine whole.
#[tokio::test]
async fn terminal_only_loop_returns_the_verdict() {
    let engine = Arc::new(Scripted::new(vec![
        gate_pick(
            "take",
            0.74,
            [("take", 0.83), ("skip", 0.15), ("ask_architect", 0.02)],
        ),
        gate_pick(
            "skip",
            0.55,
            [("take", 0.40), ("skip", 0.55), ("ask_architect", 0.05)],
        ),
    ]));
    let clock = Arc::new(SimClock::at(T0));
    let l = DecisionLoop::new(
        "xl_gate",
        gate_cfg(),
        engine.clone(),
        Arc::new(FakeTools(StdMutex::new(vec![]))),
        None,
        None,
        None,
    )
    .with_clock(clock.clone());
    let event = json!({
        "strategy": "weekend_fade", "instrument": TSLA, "side": "sell",
        "signal_bps": -182.5, "decided_at": "2026-09-27T22:00:00Z",
        "features": {"ret_24h_bps": -150.0, "hour_of_week": 166}
    });
    let take = l.decide_terminal(&event, "backtest:r1:0").await.unwrap();
    assert_eq!(
        take,
        Verdict {
            action: "take".into(),
            confidence: 0.74,
            probabilities: BTreeMap::from([
                ("ask_architect".into(), 0.02),
                ("skip".into(), 0.15),
                ("take".into(), 0.83),
            ]),
            below_act_at: false,
            outcome: StepOutcome::Stopped {
                action: "take".into()
            },
        }
    );
    clock.advance(3_600_000);
    let unsure = l.decide_terminal(&event, "backtest:r1:1").await.unwrap();
    assert!(unsure.below_act_at);
    assert_eq!(unsure.action, "skip");
    assert_eq!(unsure.probabilities["take"], 0.40);
    assert_eq!(
        unsure.outcome,
        StepOutcome::Escalated {
            action: "skip".into(),
            confidence: 0.55
        }
    );

    assert_eq!(legal_actions(&engine, 0), ["ask_architect", "skip", "take"]);
    // The confident pick is the next event's history; the escalated one is not.
    assert_eq!(l.history().await.len(), 1);
    let states = engine.states.lock().unwrap();
    assert_eq!(states.len(), 2, "one decision per event");
    assert_eq!(states[0]["event"], event);
    assert_eq!(states[1]["history"], json!([{"t": 1, "action": "take"}]));
}

#[tokio::test]
async fn decide_terminal_refuses_a_loop_with_tools_before_any_call() {
    let (l, engine, tools, _) = build(false, vec![]);
    let err = l.decide_terminal(&json!({}), "s").await.unwrap_err();
    assert!(format!("{err}").contains("terminal-only"), "{err}");
    assert!(engine.states.lock().unwrap().is_empty());
    assert!(tools.0.lock().unwrap().is_empty());
}

/// A failed decisions call (an offline cache miss) is audited, then
/// returned as the error.
#[tokio::test]
async fn decide_terminal_returns_the_engine_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("decisions.jsonl");
    let l = DecisionLoop::new(
        "xl_gate",
        gate_cfg(),
        Arc::new(Failing),
        Arc::new(FakeTools(StdMutex::new(vec![]))),
        None,
        None,
        Some(AuditLog {
            path: path.clone(),
            sandbox: None,
            trigger: Some("backtest".into()),
            ids: Default::default(),
        }),
    )
    .with_clock(Arc::new(SimClock::at(T0)));
    let err = l.decide_terminal(&json!({}), "s").await.unwrap_err();
    assert!(format!("{err:#}").contains("402"), "{err:#}");
    let line: Value = serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
    assert_eq!(line["result"]["outcome"], json!("error"));
    assert_eq!(line["ts_ms"], json!(T0));
}

// ── trace (docs/plans/TENGU_STUDIO_PLAN.md ST-12) ──────────────────────────────

use crate::application::trace_exec::tests::MemTrace;

/// `(kind, status, node)` of every event, in order.
fn shape(sink: &MemTrace) -> Vec<(String, Status, String)> {
    sink.all()
        .into_iter()
        .map(|d| (d.kind, d.status, d.node_id.unwrap_or_default()))
        .collect()
}

fn ev(kind: &str, status: Status, node: &str) -> (String, Status, String) {
    (kind.to_string(), status, node.to_string())
}

/// Each `StepOutcome` maps to its event (module doc § Trace) and
/// carries the outcome as computed; step events hang on the event's
/// cause, the action on its decision, the result on the action.
#[tokio::test]
async fn trace_covers_every_outcome() {
    use Status as S;
    // Executed (ok) → Stopped; one `world` alias (no store: failed).
    let sink = Arc::new(MemTrace::default());
    let (mut l, ..) = build(false, fetch_then_hold());
    l.cfg.world.insert("tick".into(), "feed/1:tick".into());
    let l = l.with_trace(sink.clone());
    let cause = Cause::new(Some("root:3".into()), "s1");
    trace_exec::caused_by(cause, l.handle_event(&json!({}), "s1"))
        .await
        .unwrap();
    assert_eq!(
        shape(&sink),
        [
            ev("observation.read", S::Failed, "world:t/tick"),
            ev("jev.completed", S::Ok, "jev:t"),
            ev("action.selected", S::Running, "action:t/fetch"),
            ev("action.completed", S::Ok, "action:t/fetch"),
            ev("observation.read", S::Failed, "world:t/tick"),
            ev("jev.completed", S::Ok, "jev:t"),
            ev("action.selected", S::Ok, "action:t/hold"),
        ]
    );
    let d = sink.all();
    let parent = |i: usize| d[i].parent_event_id.clone();
    assert_eq!(parent(0).as_deref(), Some("root:3"));
    assert_eq!(parent(1).as_deref(), Some("root:3"));
    assert_eq!(parent(2), Some(MemTrace::id(1)), "action ← its decision");
    assert_eq!(parent(3), Some(MemTrace::id(2)), "result ← its action");
    assert_eq!(parent(6), Some(MemTrace::id(5)));
    assert!(d.iter().all(|e| e.session_id.as_deref() == Some("s1")));
    assert_eq!(d[0].payload["key"], json!("feed/1:tick"));
    assert_eq!(d[0].payload["read"], json!("error"));
    assert_eq!(d[2].call_id.as_deref(), Some("t:s1:1"));
    assert_eq!(d[3].call_id.as_deref(), Some("t:s1:1"));
    assert_eq!(
        d[3].payload["outcome"],
        json!({"outcome": "executed", "action": "fetch"})
    );
    assert_eq!(d[3].payload["ok"], json!(true));
    assert_eq!(d[6].payload["confidence"], json!(0.99));

    // DryRun → `action.selected` skipped; Rejected → `action.rejected`.
    let sink = Arc::new(MemTrace::default());
    let (l, ..) = build(
        true,
        vec![
            pick(&[("next_action", "fetch", 0.95)]),
            pick(&[
                ("next_action", "open", 0.9),
                ("open__pool", "pool_2", 0.9),
                ("open__size", "1", 0.9),
            ]),
            pick(&[("next_action", "open", 0.9), ("open__pool", "nope", 0.9)]),
        ],
    );
    let l = l.with_trace(sink.clone());
    l.handle_event(&json!({}), "s2").await.unwrap();
    let s = shape(&sink);
    assert_eq!(s[4], ev("action.selected", S::Skipped, "action:t/open"));
    assert_eq!(s[6], ev("action.rejected", S::Refused, "action:t/open"));
    let d = sink.all();
    assert_eq!(d[4].payload["args"], json!({"pool": "B2", "size": 1}));
    assert!(d[6].payload["outcome"]["reason"]
        .as_str()
        .unwrap()
        .contains("unknown label `nope`"));
    assert_eq!(d[0].parent_event_id, None, "no cause: no root");

    // Refused (`[risk]` deny) → `action.refused` under the selection.
    let sink = Arc::new(MemTrace::default());
    let l = DecisionLoop::new(
        "entry",
        entry_cfg(),
        Arc::new(Scripted::new(vec![
            pick(&[("next_action", "enter_small", 0.95)]),
            pick(&[("next_action", "done", 0.99)]),
        ])),
        Arc::new(DenyingTools),
        None,
        None,
        None,
    )
    .with_trace(sink.clone());
    l.handle_event(&json!({}), "s3").await.unwrap();
    let s = shape(&sink);
    assert_eq!(
        s[2],
        ev("action.refused", S::Refused, "action:entry/enter_small")
    );
    let d = sink.all();
    assert_eq!(d[2].payload["outcome"]["rule"], json!("min_edge"));
    assert_eq!(d[2].parent_event_id, Some(MemTrace::id(1)));

    // Error → `jev.failed` (the error then propagates).
    let sink = Arc::new(MemTrace::default());
    let l = DecisionLoop::new(
        "t",
        cfg(false),
        Arc::new(Failing),
        Arc::new(FakeTools(StdMutex::new(vec![]))),
        None,
        None,
        None,
    )
    .with_trace(sink.clone());
    assert!(l.handle_event(&json!({}), "s4").await.is_err());
    assert_eq!(shape(&sink), [ev("jev.failed", S::Failed, "jev:t")]);
    let d = sink.all();
    assert_eq!(d[0].payload["model"], json!("~typesafe/jev-latest"));
    assert_eq!(d[0].payload["outcome"]["outcome"], json!("error"));
    assert!(d[0].duration_ms.is_some());
}

/// `jev.completed.payload.legal` is exactly what Jev was offered: the
/// `next_action` criteria, and per multi-candidate slot its labels.
#[tokio::test]
async fn trace_legal_set_matches_questions() {
    let sink = Arc::new(MemTrace::default());
    let (l, engine, ..) = build(
        false,
        vec![
            pick(&[("next_action", "fetch", 0.95)]),
            pick(&[("next_action", "hold", 0.99)]),
        ],
    );
    let l = l.with_trace(sink.clone());
    l.handle_event(&json!({}), "s").await.unwrap();
    let jev: Vec<EventDraft> = sink
        .all()
        .into_iter()
        .filter(|d| d.kind == "jev.completed")
        .collect();
    let seen = engine.seen.lock().unwrap();
    for (step, d) in jev.iter().enumerate() {
        let Question::Choice { criteria, .. } = &seen[step][NEXT_ACTION] else {
            panic!()
        };
        let legal = d.payload["legal"].as_object().unwrap();
        let offered: Vec<&String> = criteria.keys().collect();
        assert_eq!(legal.keys().collect::<Vec<_>>(), offered, "step {step}");
        // The names alone too, pinned so they survive the payload bound.
        assert_eq!(d.payload["legal_actions"], json!(offered), "step {step}");
        assert_eq!(d.keep, LEGAL_FIELDS);
        let questions: Vec<String> = seen[step].keys().cloned().collect();
        assert_eq!(d.payload["questions"], json!(questions));
        for (key, q) in seen[step].iter().filter(|(k, _)| k.contains("__")) {
            let (an, sn) = key.split_once("__").unwrap();
            let Question::Choice { criteria, .. } = q else {
                panic!()
            };
            let labels: Vec<&String> = criteria.keys().collect();
            assert_eq!(legal[an][sn], json!(labels), "{key}");
        }
    }
    // Step 1: `open` was not legal (no fetch result yet).
    assert!(jev[0].payload["legal"].get("open").is_none());
    // Step 2: legal with every slot's candidates; size capped at 2.
    assert_eq!(
        jev[1].payload["legal"]["open"],
        json!({"pool": ["pool_1", "pool_2"], "size": ["0.5", "1", "2"]})
    );
    assert_eq!(jev[1].payload["step"], json!(1));
}

/// Below `act_at`: `action.escalated` on the gate node, with the
/// confidence, the threshold and whether it was handed on.
#[tokio::test]
async fn escalation_event_has_confidence_and_act_at() {
    let sink = Arc::new(MemTrace::default());
    let (l, _, tools, esc) = build(false, vec![pick(&[("next_action", "fetch", 0.3)])]);
    let l = l.with_trace(sink.clone());
    let out = l.handle_event(&json!({}), "s").await.unwrap();
    assert!(matches!(out[0], StepOutcome::Escalated { .. }));
    assert!(tools.0.lock().unwrap().is_empty());
    assert_eq!(esc.0.lock().unwrap().len(), 1);
    let d = sink.all();
    assert_eq!(
        shape(&sink)[1],
        ev("action.escalated", Status::Escalated, "gate:t/act_at")
    );
    assert_eq!(d[1].parent_event_id, Some(MemTrace::id(0)));
    let p = &d[1].payload;
    assert_eq!(p["action"], json!("fetch"));
    assert_eq!(p["confidence"], json!(0.3));
    assert_eq!(p["act_at"], json!(0.8));
    assert_eq!(
        (p["escalate"].clone(), p["escalator"].clone()),
        (json!(true), json!(true))
    );
    assert_eq!(
        p["outcome"],
        json!({"outcome": "escalated", "action": "fetch", "confidence": 0.3})
    );
}
