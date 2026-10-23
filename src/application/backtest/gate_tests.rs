//! Tests of `src/application/backtest/gate.rs`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::*;
use crate::adapters::outbound::decision_cache::{CachedDecisionEngine, DECISION_CACHE_DB};
use crate::bootstrap::decision::{build_gate, build_replay_loop};
use crate::config::Config;
use crate::domain::backtest::engine::{candidates as rule_candidates, CandidateSet};
use crate::domain::backtest::gate::{GateClass, COST_PER_DECISION_USD};
use crate::domain::backtest::stats::paired_diff_ci;
use crate::domain::backtest::testkit::{random_market, run_params, spec, utc, H};
use crate::domain::decision::{Answer, Decision, Question};
use crate::domain::observation::{assert_features_ok, ObsSource, Observation, MAX_FEATURES};

const MODEL: &str = "~typesafe/jev-latest";
const IDS: [&str; 3] = [
    "hyperliquid:xyz:AAA",
    "hyperliquid:xyz:BBB",
    "hyperliquid:xyz:CCC",
];

/// `xl_gate` shaped like the xlab sandbox's, `history = 1` (the replay
/// loop must not carry it between candidates); `act_at` 0.6 so the fake's
/// 0.4 confidence is `unsure` (the sandbox runs 0.01).
fn config() -> Config {
    toml::from_str(
        r#"
            [agents.xl_jev]
            engine = "openrouter"
            model = "m"
            tools = ["read_file"]

            [decision_loops.xl_gate]
            goal = "Gate the trades a rule proposes"
            agent = "xl_jev"
            act_at = 0.6
            max_steps = 1
            escalate = false
            history = 1
            [decision_loops.xl_gate.actions.take]
            description = "Trade it"
            [decision_loops.xl_gate.actions.skip]
            description = "Do not trade it"
            [decision_loops.xl_gate.actions.ask_architect]
            description = "Needs research first"
            "#,
    )
    .unwrap()
}

/// A fade-the-move rule on a deterministic random market (testkit):
/// hundreds of candidates with features, signals 30 bps and up.
fn world() -> (StrategySpec, MarketData, RunParams, Vec<Candidate>) {
    let t0 = utc("2026-08-31 00:00");
    let md = random_market(&IDS, t0, 24 * 30, 11);
    let mut p = run_params(t0 + 9 * 24 * H, t0 + 28 * 24 * H);
    p.universe = IDS.iter().map(|s| s.to_string()).collect();
    let s = spec(
        json!({"kind": "move_trigger", "universe": IDS, "interval": "1h",
            "lookback_bars": 1, "threshold_bps": 30, "direction": "fade", "hold_bars": 3,
            "cooldown_bars": 2}),
    );
    let set = rule_candidates(&s, &md, &p).unwrap();
    (s, md, p, set.candidates)
}

/// The fake engine's rule on |signal_bps| (the event's, rounded):
/// (class, choice, confidence, p(take)); `None` choice = a failed call.
fn rule(signal: f64) -> (GateClass, Option<&'static str>, f64, f64) {
    let s = signal.abs();
    if s >= 120.0 {
        (GateClass::Take, Some("take"), 0.85, 0.85)
    } else if s >= 80.0 {
        (GateClass::Skip, Some("skip"), 0.8, 0.15)
    } else if s >= 60.0 {
        (GateClass::AskArchitect, Some("ask_architect"), 0.7, 0.1)
    } else if s >= 45.0 {
        (GateClass::Unsure, Some("take"), 0.4, 0.4)
    } else if s >= 38.0 {
        (GateClass::Rejected, Some("hold"), 0.9, 0.0)
    } else {
        (GateClass::Error, None, 0.0, 0.0)
    }
}

fn expected(c: &Candidate) -> GateClass {
    rule(gate_event("t", c)["signal_bps"].as_f64().unwrap()).0
}

/// Decides by signal size ([`rule`]); yields a few times so workers
/// interleave; records every state.
#[derive(Default)]
struct BySignal {
    states: StdMutex<Vec<Value>>,
}

#[async_trait]
impl DecisionEngine for BySignal {
    fn model(&self) -> &str {
        MODEL
    }
    async fn decide(&self, state: &Value, q: &BTreeMap<String, Question>) -> Result<Decision> {
        assert!(q.contains_key("next_action"), "{q:?}");
        self.states.lock().unwrap().push(state.clone());
        let signal = state["event"]["signal_bps"].as_f64().unwrap();
        for _ in 0..(signal.abs() as u64 % 4) {
            tokio::task::yield_now().await;
        }
        let (_, choice, confidence, p) = rule(signal);
        let Some(choice) = choice else {
            bail!("decisions endpoint HTTP 503: unavailable");
        };
        let mut probabilities = BTreeMap::from([("take".to_string(), p)]);
        if choice != "take" && choice != "hold" {
            probabilities.insert(choice.to_string(), confidence);
        }
        Ok(Decision {
            id: format!("gen-dec-{signal}"),
            model: "typesafe/jev-1.13-20260917".into(),
            answers: BTreeMap::from([(
                "next_action".to_string(),
                Answer {
                    kind: "choice".into(),
                    choice: Some(choice.into()),
                    probabilities,
                    confidence: Some(confidence),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        })
    }
}

/// Counts what one worker's loop asked, then asks the shared engine.
struct Counted {
    inner: Arc<dyn DecisionEngine>,
    calls: AtomicUsize,
}

#[async_trait]
impl DecisionEngine for Counted {
    fn model(&self) -> &str {
        self.inner.model()
    }
    async fn decide(&self, s: &Value, q: &BTreeMap<String, Question>) -> Result<Decision> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.decide(s, q).await
    }
}

/// K workers through `build_replay_loop` over `engine`, each behind its
/// own counter, auditing to `audit`.
fn gate(engine: Arc<dyn DecisionEngine>, k: usize, audit: &Path) -> (Gate, Vec<Arc<Counted>>) {
    let config = config();
    let mut counters = Vec::new();
    let workers = (0..k)
        .map(|_| {
            let counted = Arc::new(Counted {
                inner: Arc::clone(&engine),
                calls: AtomicUsize::new(0),
            });
            counters.push(Arc::clone(&counted));
            let clock = Arc::new(SimClock::at(0));
            let l = build_replay_loop(&config, "xl_gate", counted, clock.clone(), audit).unwrap();
            (clock, l)
        })
        .collect();
    let gate = Gate {
        loop_name: "xl_gate".into(),
        engine,
        workers,
    };
    (gate, counters)
}

fn audit_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Classes follow the verdicts; the first `max_decisions` by seq are
/// decided, the rest counted as cut; every call is audited at its
/// candidate's instant.
#[tokio::test]
async fn classes_cut_and_audit_at_the_decision_instant() {
    let (_, _, _, cands) = world();
    let n = cands.len();
    assert!(n > 100, "{n} candidates");
    let dir = tempfile::tempdir().unwrap();
    let audit = dir.path().join("run/decisions.jsonl");
    let (g, _) = gate(Arc::new(BySignal::default()), 2, &audit);
    // Shuffled input: the order is the seq, not the slice.
    let mut shuffled = cands.clone();
    shuffled.reverse();
    let run = run_gate(&shuffled, "t", &g, n - 5).await.unwrap();
    assert_eq!((run.decided, run.cut), (n - 5, 5));
    assert_eq!(
        run.decisions.iter().map(|d| d.seq).collect::<Vec<_>>(),
        (0..n - 5).collect::<Vec<_>>(),
        "the earliest are decided"
    );
    let mut seen = BTreeSet::new();
    for d in &run.decisions {
        assert_eq!(d.class, expected(&cands[d.seq]), "{}", describe(d));
        seen.insert(d.class);
        match d.class {
            GateClass::Error => {
                assert!(d.error.as_deref().unwrap().contains("HTTP 503"), "{d:?}");
                assert!(d.verdict.is_none());
            }
            _ => assert!(d.verdict.is_some() && d.error.is_none()),
        }
    }
    assert_eq!(
        seen.len(),
        GateClass::ALL.len(),
        "every class occurs: {seen:?}"
    );
    assert_eq!(run.cache, None, "no caching engine");
    assert_eq!(
        (run.model.as_str(), run.loop_name.as_str()),
        (MODEL, "xl_gate")
    );
    // One audit line per call, at the candidate's instant.
    let lines = audit_lines(&audit);
    assert_eq!(lines.len(), n - 5);
    for line in &lines {
        let session = line["session_id"].as_str().unwrap();
        let seq: usize = session
            .strip_prefix("backtest:t:")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(line["ts_ms"], json!(cands[seq].decided_at_ms), "{line}");
        assert_eq!(line["trigger"], json!("backtest"));
    }
    // Nothing asked: everything cut.
    let none = run_gate(&cands, "t", &g, 0).await.unwrap();
    assert_eq!((none.decided, none.cut), (0, n));
    assert_eq!(audit_lines(&audit).len(), n - 5);
}

/// K = 1 and K = 4: the same requests per candidate (no history carried
/// between candidates), the same decisions in the same order; all four
/// workers took part.
#[tokio::test]
async fn the_worker_count_changes_nothing() {
    let (_, _, _, cands) = world();
    let dir = tempfile::tempdir().unwrap();
    let audit = dir.path().join("decisions.jsonl");
    let one = Arc::new(BySignal::default());
    let (g1, _) = gate(one.clone(), 1, &audit);
    let r1 = run_gate(&cands, "t", &g1, usize::MAX).await.unwrap();
    let four = Arc::new(BySignal::default());
    let (g4, counters) = gate(four.clone(), 4, &audit);
    let r4 = run_gate(&cands, "t", &g4, usize::MAX).await.unwrap();
    assert_eq!(r1.decisions, r4.decisions);
    assert_eq!(r1.decided, cands.len());
    for c in &counters {
        assert!(c.calls.load(Ordering::SeqCst) > 0, "a worker did nothing");
    }
    let sorted = |e: &BySignal| {
        let mut v: Vec<String> = e
            .states
            .lock()
            .unwrap()
            .iter()
            .map(Value::to_string)
            .collect();
        v.sort();
        v
    };
    assert_eq!(sorted(&one), sorted(&four), "identical requests");
    for s in four.states.lock().unwrap().iter() {
        assert_eq!(s["history"], json!([]), "{s}");
        assert_eq!(s["step"], json!(0));
    }
}

/// Offline, every miss is a class-error decision and the run goes on;
/// online then offline, the rerun is answered from the cache with any K.
#[tokio::test]
async fn offline_misses_are_errors_and_reruns_hit_the_cache() {
    let (s, md, p, cands) = world();
    let cands: Vec<Candidate> = cands.into_iter().take(40).collect();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("backtests").join(DECISION_CACHE_DB);
    let audit = dir.path().join("decisions.jsonl");

    let offline: Arc<dyn DecisionEngine> =
        Arc::new(CachedDecisionEngine::open(&db, MODEL, None).unwrap());
    let (g, _) = gate(offline, 2, &audit);
    let dry = run_gate(&cands[..6], "t", &g, 500).await.unwrap();
    assert_eq!(dry.decided, 6);
    assert!(dry.decisions.iter().all(|d| d.class == GateClass::Error));
    assert!(dry.decisions[0]
        .error
        .as_deref()
        .unwrap()
        .contains("offline"));
    assert_eq!(
        dry.cache,
        Some(CacheStats {
            hits: 0,
            misses: 6,
            errors: 6
        })
    );
    let arms = gate_arms(&dry, &cands, &s, &md, &p, None, 0.0);
    assert_eq!((arms.summary.errors, arms.summary.take_rate), (6, None));
    assert_eq!(arms.summary.est_cost_usd, 0.0);
    assert!(arms.jev.trades.is_empty() && arms.taken.is_empty());

    // Online over the fake, then offline with another K.
    let fake = Arc::new(BySignal::default());
    let online: Arc<dyn DecisionEngine> =
        Arc::new(CachedDecisionEngine::open(&db, MODEL, Some(fake.clone())).unwrap());
    let (g, _) = gate(online, 3, &audit);
    let live = run_gate(&cands, "t", &g, 500).await.unwrap();
    let failed = live
        .decisions
        .iter()
        .filter(|d| d.class == GateClass::Error)
        .count();
    let answered = (cands.len() - failed) as u64;
    let stats = live.cache.unwrap();
    assert_eq!((stats.hits, stats.misses), (0, cands.len() as u64));
    let offline: Arc<dyn DecisionEngine> =
        Arc::new(CachedDecisionEngine::open(&db, MODEL, None).unwrap());
    let (g, _) = gate(offline, 4, &audit);
    let rerun = run_gate(&cands, "t", &g, 500).await.unwrap();
    let stats = rerun.cache.unwrap();
    assert_eq!(
        (stats.hits, stats.misses),
        (answered, failed as u64),
        "answered calls are cached; failed ones were never stored"
    );
    for (a, b) in live.decisions.iter().zip(&rerun.decisions) {
        assert_eq!((a.seq, a.class), (b.seq, b.class));
        assert_eq!(a.verdict, b.verdict);
    }
}

/// Rules = every decided candidate, jev = the taken ones (the research
/// trades of the same candidates); calibration joins by seq; the report
/// gets the arms, jev − rules first and the calibration; the row stays
/// within 32 features.
#[tokio::test]
async fn arms_summary_and_report() {
    let (s, md, p, cands) = world();
    let dir = tempfile::tempdir().unwrap();
    let (g, _) = gate(
        Arc::new(BySignal::default()),
        2,
        &dir.path().join("decisions.jsonl"),
    );
    let run = run_gate(&cands, "t", &g, usize::MAX).await.unwrap();
    let caps = RiskCaps {
        initial_cash_usd: 100.0,
        max_order_notional_usd: 25.0,
        max_gross_exposure_usd: 110.0,
        max_net_exposure_usd: 110.0,
        daily_loss_limit_usd: 10.0,
        total_loss_limit_usd: 25.0,
    };
    let arms = gate_arms(
        &run,
        &cands,
        &s,
        &md,
        &p,
        Some(&caps),
        COST_PER_DECISION_USD,
    );
    let taken: Vec<usize> = run
        .decisions
        .iter()
        .filter(|d| d.class == GateClass::Take)
        .map(|d| d.seq)
        .collect();
    assert_eq!(arms.taken, taken);
    assert_eq!(arms.decided.len(), cands.len());
    // The jev arm's trades are the rules arm's trades of the taken seqs.
    let rules_by_seq: BTreeMap<usize, f64> = arms
        .rules
        .trades
        .iter()
        .map(|t| (t.seq, t.net_bps))
        .collect();
    assert!(!arms.jev.trades.is_empty());
    for t in &arms.jev.trades {
        assert!(taken.contains(&t.seq));
        assert_eq!(rules_by_seq[&t.seq], t.net_bps);
    }
    // Calibration: answered, not rejected, with a rules trade.
    let points = run
        .decisions
        .iter()
        .filter(|d| !matches!(d.class, GateClass::Error | GateClass::Rejected))
        .filter(|d| rules_by_seq.contains_key(&d.seq))
        .count();
    let sum = &arms.summary;
    assert_eq!(sum.calibration.n, points);
    assert!(sum.calibration.brier.is_some());
    assert_eq!(
        sum.diff_ci,
        paired_diff_ci(&arms.jev.trades, &arms.rules.trades, p.bootstrap, p.seed)
    );
    assert!(sum.capped && sum.diff_ci_capped.is_some() && arms.rules_capped.is_some());
    assert_eq!(sum.take, taken.len());
    assert_eq!(sum.est_cost_usd, 0.0, "no cache, no misses counted");

    // Into the report.
    let set = CandidateSet {
        candidates: cands.clone(),
        ..Default::default()
    };
    let mut report =
        BacktestReport::new("run-1", &s, s.to_value(), "ab".repeat(32), &p, None, &set);
    let research = simulate(&s, &md, &p, &cands, Arm::Research);
    report.add_arm("research", cands.len(), &research);
    arms.add_to_report(&mut report);
    assert_eq!(
        report.arms.keys().map(String::as_str).collect::<Vec<_>>(),
        ["jev", "jev_capped", "research", "rules", "rules_capped"]
    );
    assert_eq!(report.arms["jev"].n_candidates, taken.len());
    assert_eq!(
        (
            report.comparisons[0].a.as_str(),
            report.comparisons[0].b.as_str()
        ),
        ("jev", "rules")
    );
    assert_eq!(report.comparisons[1].a, "jev_capped");
    assert_eq!(report.calibration.as_ref(), Some(&sum.calibration));
    assert_eq!(report.gate.as_ref(), Some(sum));
    // The report alone renders the gate: its section, CLI lines, row.
    let md_text = report.render_markdown();
    for want in [
        "| jev − rules |",
        "## Calibration (n",
        "## Jev gate `xl_gate`",
    ] {
        assert!(md_text.contains(want), "missing `{want}`");
    }
    assert_eq!(md_text.matches("## Jev gate").count(), 1);
    assert!(report
        .render_compact()
        .contains(&sum.render_compact().lines().next().unwrap().to_string()));
    let obs = Observation::of("backtest", &report, 0, 0, ObsSource::Live);
    assert_eq!(
        obs.features["diff_bps"],
        json!(sum.diff_ci.as_ref().unwrap().diff_bps)
    );
    assert!(obs.features.len() <= MAX_FEATURES);
    assert!(obs.features.contains_key("jev_take_rate"));
    assert_features_ok(&obs.features);
}

/// The audit moves into the run dir by seq, lines verbatim; a line
/// without a parsable session keeps its place after them; the temp file
/// is gone once the audit drops.
#[test]
fn the_audit_is_ordered_by_seq_and_removed() {
    let audit = GateAudit::new().unwrap();
    let path = audit.path().to_path_buf();
    assert!(!path.starts_with(std::env::current_dir().unwrap()));
    let line = |seq: &str| format!(r#"{{"session_id":"backtest:w:{seq}","ts_ms":1}}"#);
    let text = [
        line("10"),
        line("2"),
        "not json".to_string(),
        line("0"),
        r#"{"session_id":"backtest:other:1"}"#.to_string(),
        String::new(),
        line("1"),
    ]
    .join("\n");
    std::fs::write(&path, text).unwrap();
    let sorted = audit.lines_by_seq("w").unwrap();
    assert_eq!(
        sorted,
        [
            line("0"),
            line("1"),
            line("2"),
            line("10"),
            "not json".to_string(),
            r#"{"session_id":"backtest:other:1"}"#.to_string(),
        ]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect::<String>()
    );
    drop(audit);
    assert!(!path.exists());
    assert_eq!(DECISIONS_FILE, "decisions.jsonl");
}

#[tokio::test]
async fn runs_that_cannot_start_say_why() {
    let (_, _, _, cands) = world();
    let empty = Gate {
        loop_name: "xl_gate".into(),
        engine: Arc::new(BySignal::default()),
        workers: Vec::new(),
    };
    let err = run_gate(&cands, "t", &empty, 10).await.unwrap_err();
    assert!(format!("{err}").contains("no worker"), "{err}");
    let dir = tempfile::tempdir().unwrap();
    let (g, _) = gate(
        Arc::new(BySignal::default()),
        1,
        &dir.path().join("d.jsonl"),
    );
    let twice = vec![cands[0].clone(), cands[0].clone()];
    let err = run_gate(&twice, "t", &g, 10).await.unwrap_err();
    assert!(format!("{err}").contains("seq 0 appears twice"), "{err}");
    assert_eq!(session_id("weekend_fade", 12), "backtest:weekend_fade:12");
}

// ── live (OPENROUTER_API_KEY; spends < $0.01) ─────────────────────────

/// Ten weekend-fade-like candidates (Sun 2026-09-27 22:00 UTC), signals
/// from −420 to +510 bps, plausible features.
fn synthetic() -> Vec<Candidate> {
    use crate::domain::backtest::engine::{ExitPlan, Leg};
    use crate::domain::book::Side;
    let t = utc("2026-09-27 22:00");
    let rows: [(&str, f64, f64, f64, f64, f64); 10] = [
        // id, signal, vol_24h, volume_ratio, trades_last_bar, half_spread
        ("hyperliquid:xyz:TSLA", -420.0, 85.0, 2.4, 310.0, 1.0),
        ("hyperliquid:xyz:NVDA", -260.0, 60.0, 1.1, 280.0, 1.0),
        ("hyperliquid:xyz:AAPL", -180.0, 35.0, 0.9, 150.0, 1.0),
        ("hyperliquid:xyz:MSTR", -95.0, 140.0, 0.6, 40.0, 3.5),
        ("hyperliquid:xyz:HOOD", -40.0, 70.0, 0.4, 12.0, 6.0),
        ("hyperliquid:xyz:COIN", 35.0, 90.0, 0.8, 55.0, 2.0),
        ("hyperliquid:xyz:PLTR", 80.0, 75.0, 1.3, 95.0, 1.5),
        ("hyperliquid:xyz:AMD", 150.0, 55.0, 3.1, 220.0, 1.0),
        ("hyperliquid:xyz:META", 240.0, 45.0, 0.7, 180.0, 1.0),
        ("hyperliquid:xyz:BIRD", 510.0, 320.0, 6.0, 3.0, 25.0),
    ];
    rows.iter()
        .enumerate()
        .map(|(seq, &(id, signal, vol, ratio, trades, spread))| {
            let side = if signal > 0.0 { Side::Sell } else { Side::Buy };
            Candidate {
                seq,
                instrument: id.into(),
                side,
                legs: vec![Leg {
                    instrument: id.into(),
                    side,
                    entry_px: 100.0,
                }],
                signal_bps: signal,
                decided_at_ms: t,
                data_asof_ms: t,
                period: "2026-09-25".into(),
                anchor_px: None,
                exit: ExitPlan::At {
                    exit_ms: utc("2026-09-28 13:00"),
                },
                features: BTreeMap::from([
                    ("ret_1h_bps".into(), signal / 12.0),
                    ("ret_24h_bps".into(), signal * 0.8),
                    ("ret_168h_bps".into(), -signal * 0.5),
                    ("vol_24h_bps".into(), vol),
                    ("volume_ratio_24h".into(), ratio),
                    ("trades_last_bar".into(), trades),
                    ("funding_apr_pct".into(), 10.95),
                    ("half_spread_bps".into(), spread),
                    ("hour_of_week".into(), 166.0),
                ]),
                label: None,
                info_label: None,
            }
        })
        .collect()
}

fn xlab() -> Config {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("sandboxes/xlab/config.toml");
    Config::load(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()))
}

fn print_run(run: &GateRun, cands: &[Candidate], audit: &Path) {
    for d in &run.decisions {
        println!("{} {}", cands[d.seq].instrument, describe(d));
    }
    let lines = audit_lines(audit);
    let cost: f64 = lines
        .iter()
        .filter_map(|l| l.pointer("/usage/cost").and_then(Value::as_f64))
        .sum();
    let models: BTreeSet<String> = lines
        .iter()
        .filter_map(|l| l["model"].as_str().map(str::to_string))
        .collect();
    println!(
            "cache {:?} · est ${:.5} at ${COST_PER_DECISION_USD}/miss · usage.cost Σ ${cost:.6} over {} audit lines · models {models:?}",
            run.cache,
            run.cache.map_or(0, |c| c.misses) as f64 * COST_PER_DECISION_USD,
            lines.len()
        );
}

/// `[decision_loops.xl_gate]` of `sandboxes/xlab/config.toml` over the
/// real cached `JevClient` on a temp state dir: 10 live decisions that
/// all parse into a class, then an offline rerun with another K answered
/// wholly from the cache. Run:
/// `cargo test --bin tengu live_gate_on_synthetic_candidates -- --ignored --nocapture`.
#[tokio::test]
#[ignore]
async fn live_gate_on_synthetic_candidates() {
    let config = xlab();
    let cands = synthetic();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let audit = dir.path().join("run/decisions.jsonl");
    let g = build_gate(&config, "xl_gate", &state, &audit, 2, false).unwrap();
    let run = run_gate(&cands, "weekend_fade", &g, 500).await.unwrap();
    print_run(&run, &cands, &audit);
    assert_eq!(run.decided, 10);
    for d in &run.decisions {
        assert!(d.verdict.is_some(), "{}", describe(d));
        assert!(
            matches!(
                d.class,
                GateClass::Take
                    | GateClass::Skip
                    | GateClass::AskArchitect
                    | GateClass::Unsure
                    | GateClass::Rejected
            ),
            "{}",
            describe(d)
        );
    }
    assert_eq!(run.cache.map(|c| (c.hits, c.misses)), Some((0, 10)));

    let again = build_gate(&config, "xl_gate", &state, &audit, 4, true).unwrap();
    let rerun = run_gate(&cands, "weekend_fade", &again, 500).await.unwrap();
    print_run(&rerun, &cands, &audit);
    assert_eq!(rerun.cache.map(|c| (c.hits, c.misses)), Some((10, 0)));
    assert_eq!(rerun.decisions, run.decisions, "the cache replays them");
}

/// The pinned build slug the decisions endpoint reports
/// (`typesafe/jev-1.13-20260917`) through the same gate: is it accepted?
/// `cargo test --bin tengu live_gate_with_a_pinned_slug -- --ignored --nocapture`.
#[tokio::test]
#[ignore]
async fn live_gate_with_a_pinned_slug() {
    let mut config = xlab();
    config.decision_loops.get_mut("xl_gate").unwrap().model = "typesafe/jev-1.13-20260917".into();
    let cands: Vec<Candidate> = synthetic().into_iter().take(2).collect();
    let dir = tempfile::tempdir().unwrap();
    let audit = dir.path().join("decisions.jsonl");
    let g = build_gate(
        &config,
        "xl_gate",
        &dir.path().join("state"),
        &audit,
        1,
        false,
    )
    .unwrap();
    let run = run_gate(&cands, "weekend_fade", &g, 500).await.unwrap();
    print_run(&run, &cands, &audit);
    assert_eq!(run.model, "typesafe/jev-1.13-20260917");
    for d in &run.decisions {
        assert!(d.verdict.is_some(), "{}", describe(d));
    }
}
