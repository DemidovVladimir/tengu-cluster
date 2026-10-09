//! The SOE tools inside a real `run_cycle` (`application/soe/cycle.rs`): a
//! stage runner that calls them as an agent would ([`ToolStages`]), on the
//! application tests' in-memory store and on disk, and the fixture state
//! root the bridge conformance cases and the engine matrix seed.
//!
//! | Fixture | Holds | Built by |
//! |---|---|---|
//! | `tests/fixtures/soe/state/` | a state root: `cycles/2026-W42/` open in `PROPOSE` (the W41 candidate carried), `replays/fixture.w42/` open in `CHALLENGE` (one proposal), `episodes.jsonl` (W41's), generation `UNBOUND` — synthetic records and profile only | [`fixture_files`]: three cycles over `FsCycleStore` (W41 live, W42 live snapshot at its Architect stage, a W42 replay snapshot at its Critic stage) |
//! | `tests/fixtures/soe/drafts/` | `proposal.json` (the `strong_news_weak_demand` draft, ids resolved) · `challenge.json` (`soe_challenge`'s flat arguments) | same |
//! | `skills/soe-architect/resources/proposal.example.json` | the same draft: the Architect's shape example (`skill_resource`) | same |
//!
//! Regenerate both after a cycle or record change:
//! `TENGU_REGEN_SOE_STATE=1 cargo test --bin tengu fixture_state_is_current`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use super::view::{packet_page, prior_episodes, ViewRow};
use super::*;
use crate::adapters::outbound::soe::runner::skill_sha256_of;
use crate::adapters::outbound::soe::store::FsCycleStore;
use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
use crate::application::soe::cycle::{CycleEnv, CycleParams, ProfileIn, Target, PORTFOLIO};
use crate::application::soe::submit::{self as submit_mod, submit_proposal, GenerationPin};
use crate::application::soe::tests::{
    build_records, drafts, load_case, Bench, FnRunner, MemCycleStore, CLOCK_MS,
};
use crate::config::soe::SoeConfig;
use crate::domain::observation::Observation;
use crate::domain::soe::challenge::Challenge;
use crate::domain::soe::episode::OpportunityEpisode;
use crate::domain::soe::ops::Stage;
use crate::domain::soe::portfolio::WeeklyPortfolio;
use crate::domain::soe::proposal::MechanismProposal;
use crate::domain::soe::record::from_json;
use crate::domain::source::testkit::{rec, Src};
use crate::domain::source::FENCE_NOTE;
use crate::ports::clock::SimClock;
use crate::ports::runtime::Unleased;
use crate::ports::soe::{StageReply, StageRequest, StageRunner, StateLog, CHALLENGES, PROPOSALS};
use crate::ports::tool::{Tool, ToolOutput};

fn t(s: &str) -> i64 {
    s.parse::<Time>().unwrap().earliest().unwrap()
}

/// When the tools stamp (`proposed_at`): an hour after the bench clock.
const NOW_MS: i64 = CLOCK_MS + 3_600_000;
const W41: &str = "2026-10-05T12:00:00Z";
const W42: &str = "2026-10-12T12:00:00Z";
/// The fixture state's unbound pin digest (`head.json`).
const FIXTURE_PIN: &str = "00000000000000000000000000000000000000000000000000000000000000f2";

fn shared(
    store: Arc<dyn CycleStore>,
    agent: Option<&str>,
    soe: Option<SoeConfig>,
    generation: &str,
    ws: &Path,
) -> SoeShared {
    SoeShared {
        store: Ok(store),
        sandbox: Arc::new(SandboxSections {
            soe: soe.map(Arc::new),
            ..SandboxSections::default()
        }),
        clock: Arc::new(SimClock::at(NOW_MS)),
        stamp: Stamp {
            agent: agent.map(String::from),
            model: "synthetic-model".into(),
            engine: "openrouter".into(),
            skill_packages: Vec::new(),
            workspace: ws.to_path_buf(),
            step_skill_sha256: None,
            generation: generation.into(),
        },
    }
}

fn by_name(s: &SoeShared) -> BTreeMap<String, Arc<dyn Tool>> {
    tools(s)
        .into_iter()
        .map(|t| (t.definition().name.clone(), t))
        .collect()
}

/// One call of a family tool with `ws` as the workspace.
async fn call(
    tools: &BTreeMap<String, Arc<dyn Tool>>,
    ws: &Path,
    tool: &str,
    args: Value,
    call_id: &str,
) -> Result<ToolOutput> {
    let h = TestHarness::new(ws);
    let mut ctx = h.ctx();
    ctx.call_id = Some(call_id);
    tools[tool].execute(&args, &ctx).await
}

fn text_of(r: Result<ToolOutput>) -> String {
    match r {
        Ok(o) => o.text,
        Err(e) => panic!("{e:#}"),
    }
}

fn err_of(r: Result<ToolOutput>) -> String {
    match r {
        Ok(o) => panic!("expected a refusal, got:\n{}", o.text),
        Err(e) => format!("{e:#}"),
    }
}

/// One call's answer: stage, tool, text or error, observation.
type Answer = (Stage, String, Result<String, String>, Option<Observation>);

/// A stage runner that calls the family's tools as an agent would: each
/// stage's calls in order, `run` = the request's run dir unless given.
struct ToolStages {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    ws: PathBuf,
    architect: Vec<(&'static str, Value)>,
    critic: Vec<(&'static str, Value)>,
    /// Every call's answer: stage, tool, text or error, observation.
    answers: Mutex<Vec<Answer>>,
}

impl ToolStages {
    fn new(
        s: &SoeShared,
        ws: &Path,
        architect: Vec<(&'static str, Value)>,
        critic: Vec<(&'static str, Value)>,
    ) -> Self {
        Self {
            tools: by_name(s),
            ws: ws.to_path_buf(),
            architect,
            critic,
            answers: Mutex::new(Vec::new()),
        }
    }

    fn answers(&self) -> Vec<Answer> {
        self.answers.lock().unwrap().clone()
    }
}

#[async_trait]
impl StageRunner for ToolStages {
    async fn run(&self, req: &StageRequest) -> Result<StageReply> {
        let calls = match req.stage {
            Stage::Architect => &self.architect,
            _ => &self.critic,
        };
        for (i, (tool, args)) in calls.iter().enumerate() {
            let mut a = args.clone();
            if a.get("run").is_none() {
                a["run"] = req.dir.to_string().into();
            }
            let id = format!("{}:{:?}:{}", req.dir, req.stage, i + 1);
            let out = call(&self.tools, &self.ws, tool, a, &id).await;
            let (text, obs) = match out {
                Ok(o) => (Ok(o.text), o.observation),
                Err(e) => (Err(format!("{e:#}")), None),
            };
            self.answers
                .lock()
                .unwrap()
                .push((req.stage, tool.to_string(), text, obs));
        }
        Ok(StageReply {
            ok: true,
            summary: "tools ran".into(),
            ..StageReply::default()
        })
    }
}

/// The strong-news case: its stored records and its one draft.
fn strong_news() -> (Vec<crate::domain::source::SourceRecord>, Value) {
    let case = load_case("strong_news_weak_demand");
    let (by, stored) = build_records(&case.records);
    let draft = serde_json::from_str(&drafts(&case.proposals, &by)[0]).unwrap();
    (stored, draft)
}

fn bench_with(records: &[crate::domain::source::SourceRecord]) -> Bench {
    let bench = Bench::new();
    bench.sources.add(records);
    bench
}

fn mem(bench: &Bench) -> Arc<dyn CycleStore> {
    bench.store.clone() as Arc<MemCycleStore> as Arc<dyn CycleStore>
}

fn lines_of(files: &BTreeMap<String, Vec<u8>>, name: &str) -> Vec<String> {
    files
        .get(name)
        .map(|b| {
            String::from_utf8_lossy(b)
                .lines()
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The tools inside a cycle
// ---------------------------------------------------------------------------

/// `soe_propose` in a live cycle's Architect stage writes one stamped row
/// (`<cycle>.p01`) through the tools' path; the week decides on it as on a
/// recorded draft (the case's `HOLD` + cheap test).
#[tokio::test]
async fn propose_appends_one_row() {
    let (stored, draft) = strong_news();
    let bench = bench_with(&stored);
    let ws = tempfile::TempDir::new().unwrap();
    let s = shared(
        mem(&bench),
        Some("soe-architect"),
        None,
        "SOE-G0",
        ws.path(),
    );
    let stages = ToolStages::new(
        &s,
        ws.path(),
        vec![("soe_propose", json!({ "proposal": draft }))],
        Vec::new(),
    );
    let p = bench.params("2026-W41", t(W41), Target::Cycle);
    let out = bench.cycle(Some(&stages), &p).await.unwrap();
    let answers = stages.answers();
    assert_eq!(answers.len(), 1, "{answers:#?}");
    let text = answers[0].2.as_ref().unwrap();
    assert!(
        text.starts_with(
            "soe_propose 2026-W41.p01 news-automation v1 AUTOMATE run=cycles/2026-W41 | written\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("verdict HOLD") && text.contains("SINGLE_DEMAND_SIGNAL"),
        "{text}"
    );
    assert!(text.len() <= TEXT_BUDGET, "{} bytes", text.len());
    let files = bench.store.dir(&out.dir);
    let rows = lines_of(&files, PROPOSALS);
    assert_eq!(rows.len(), 1);
    let row: MechanismProposal = from_json(&rows[0]).unwrap();
    assert_eq!(
        (row.id.as_str(), row.cycle_id.as_str()),
        ("2026-W41.p01", "2026-W41")
    );
    let v = &row.provenance;
    assert_eq!(
        (
            v.agent.as_str(),
            v.model.as_str(),
            v.engine.as_str(),
            v.generation.as_str(),
            v.call_id.as_str(),
            v.proposed_at,
        ),
        (
            "soe-architect",
            "synthetic-model",
            "openrouter",
            "SOE-G0",
            "cycles/2026-W41:Architect:1",
            Time::At(NOW_MS),
        )
    );
    assert_eq!(v.skill_sha256, skill_sha256_of(&[], ws.path()));
    let portfolio: WeeklyPortfolio = serde_json::from_slice(&files[PORTFOLIO]).unwrap();
    assert_eq!(
        portfolio
            .held
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        ["news-automation"]
    );
}

/// Outside its window `soe_propose` writes nothing: during the Critic stage,
/// after the freeze, in a run nobody claimed — `stage_closed` each time.
#[tokio::test]
async fn propose_refuses_closed_cycle() {
    let (stored, draft) = strong_news();
    let bench = bench_with(&stored);
    let ws = tempfile::TempDir::new().unwrap();
    let s = shared(
        mem(&bench),
        Some("soe-architect"),
        None,
        "SOE-G0",
        ws.path(),
    );
    let stages = ToolStages::new(
        &s,
        ws.path(),
        vec![("soe_propose", json!({ "proposal": draft.clone() }))],
        vec![("soe_propose", json!({ "proposal": draft.clone() }))],
    );
    let p = bench.params("2026-W41", t(W41), Target::Cycle);
    let out = bench.cycle(Some(&stages), &p).await.unwrap();
    let answers = stages.answers();
    let late = answers[1].2.as_ref().unwrap_err();
    assert!(
        late.contains("refused the draft (1 problem(s); nothing was written)")
            && late.contains("- stage_closed: cycles/2026-W41 is in phase Challenge"),
        "{late}"
    );
    let tools = by_name(&s);
    let frozen = err_of(
        call(
            &tools,
            ws.path(),
            "soe_propose",
            json!({"run": "cycles/2026-W41", "proposal": draft.clone()}),
            "x:1",
        )
        .await,
    );
    assert!(
        frozen.contains("stage_closed: cycles/2026-W41 is not an open run"),
        "{frozen}"
    );
    let absent = err_of(
        call(
            &tools,
            ws.path(),
            "soe_propose",
            json!({"run": "cycles/2026-W40", "proposal": draft}),
            "x:2",
        )
        .await,
    );
    assert!(absent.contains("stage_closed"), "{absent}");
    assert_eq!(bench.store.proposals(&out.dir).unwrap().len(), 1);
    // A review dir, or no run dir at all, is no `run`.
    for bad in ["reviews/2026-11-02", "2026-W41", "cycles/../x"] {
        let e = err_of(
            call(
                &tools,
                ws.path(),
                "soe_propose",
                json!({"run": bad, "proposal": {}}),
                "x:3",
            )
            .await,
        );
        assert!(e.contains("is no run dir"), "{bad}: {e}");
    }
}

/// The preview is computed, never written: the only file the call adds is
/// its one `proposals.jsonl` line; previewing again changes nothing; a
/// refused draft (a computed field) adds nothing at all.
#[tokio::test(flavor = "multi_thread")]
async fn propose_preview_is_read_only() {
    let (stored, draft) = strong_news();
    let bench = bench_with(&stored);
    let ws = tempfile::TempDir::new().unwrap();
    let store = mem(&bench);
    let s = shared(Arc::clone(&store), Some("a"), None, "SOE-G0", ws.path());
    let tools = by_name(&s);
    let seen: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut cheat = draft.clone();
    cheat["verdict"] = "PASS".into();
    let probe = FnRunner(|req: &StageRequest| {
        if req.stage != Stage::Architect {
            return;
        }
        let before = bench.store.dir(&req.dir);
        let rt = tokio::runtime::Handle::current();
        let refused = tokio::task::block_in_place(|| {
            rt.block_on(call(
                &tools,
                ws.path(),
                "soe_propose",
                json!({"run": req.dir.to_string(), "proposal": cheat.to_string()}),
                "p:0",
            ))
        });
        let e = err_of(refused);
        assert!(e.contains("- computed_field: verdict"), "{e}");
        assert_eq!(
            bench.store.dir(&req.dir),
            before,
            "a refusal writes nothing"
        );
        let ok = tokio::task::block_in_place(|| {
            rt.block_on(call(
                &tools,
                ws.path(),
                "soe_propose",
                json!({"run": req.dir.to_string(), "proposal": draft.to_string()}),
                "p:1",
            ))
        });
        seen.lock().unwrap().push(text_of(ok));
        let after = bench.store.dir(&req.dir);
        let mut added: Vec<&String> = after.keys().filter(|k| !before.contains_key(*k)).collect();
        added.sort();
        assert_eq!(added, [PROPOSALS]);
        for (k, v) in &before {
            assert_eq!(&after[k], v, "{k} unchanged");
        }
        // The preview again, straight from the run: same answer, no write.
        let h = submit_mod::head(&*store, &req.dir).unwrap();
        let p = store.proposals(&req.dir).unwrap();
        let a = super::propose::assess_in_run(&*store, &req.dir, &h, p[0].opportunity()).unwrap();
        let b = super::propose::assess_in_run(&*store, &req.dir, &h, p[0].opportunity()).unwrap();
        assert_eq!(a, b);
        assert_eq!(bench.store.dir(&req.dir), after);
    });
    let p = bench.params("2026-W41", t(W41), Target::Cycle);
    bench.cycle(Some(&probe), &p).await.unwrap();
    let texts = seen.lock().unwrap().clone();
    assert_eq!(texts.len(), 1);
    assert!(
        texts[0]
            .contains("preview (read-only — computed now, before the Critic; the week decides):"),
        "{}",
        texts[0]
    );
}

/// `soe_challenge` takes flat arguments; a target that is none of the
/// week's candidates is refused (`unknown_target`), as are fields an effect
/// does not take; a valid one is `<cycle>.c01` and the week merges it.
#[tokio::test]
async fn challenge_unknown_proposal_refused() {
    let (stored, draft) = strong_news();
    let bench = bench_with(&stored);
    let ws = tempfile::TempDir::new().unwrap();
    let s = shared(mem(&bench), Some("soe-critic"), None, "SOE-G0", ws.path());
    let widen = |target: &str| {
        json!({"target": target, "kind": "HIDDEN_LABOR", "claim": "support hides hours",
               "evidence": [], "effect": "WIDEN", "field": "economics.owner_hours_per_month",
               "high": "20"})
    };
    let stages = ToolStages::new(
        &s,
        ws.path(),
        vec![("soe_propose", json!({ "proposal": draft }))],
        vec![
            ("soe_challenge", widen("nobody")),
            (
                "soe_challenge",
                json!({"target": "news-automation", "kind": "BASE_RATE", "claim": "c",
                       "effect": "NONE", "gate": "DILIGENCE_OPEN"}),
            ),
            ("soe_challenge", widen("news-automation")),
        ],
    );
    let p = bench.params("2026-W41", t(W41), Target::Cycle);
    let out = bench.cycle(Some(&stages), &p).await.unwrap();
    let answers = stages.answers();
    assert_eq!(answers.len(), 4, "{answers:#?}");
    let unknown = answers[1].2.as_ref().unwrap_err();
    assert!(unknown.contains("- unknown_target:"), "{unknown}");
    let mismatched = answers[2].2.as_ref().unwrap_err();
    assert!(
        mismatched.contains("'gate' does not go with effect NONE"),
        "{mismatched}"
    );
    let ok = answers[3].2.as_ref().unwrap();
    assert!(
        ok.starts_with(
            "soe_challenge 2026-W41.c01 target=news-automation kind=HIDDEN_LABOR effect=WIDEN \
             economics.owner_hours_per_month run=cycles/2026-W41 | written"
        ),
        "{ok}"
    );
    let files = bench.store.dir(&out.dir);
    let rows = lines_of(&files, CHALLENGES);
    assert_eq!(rows.len(), 1);
    let c: Challenge = from_json(&rows[0]).unwrap();
    assert_eq!(
        (c.id.as_str(), c.provenance.agent.as_str()),
        ("2026-W41.c01", "soe-critic")
    );
    // The merge widened the owner hours' high end (the week's answer).
    let memo = String::from_utf8_lossy(&files["memo.md"]).to_string();
    assert!(memo.contains("2026-W41.c01"), "{memo}");
}

/// The Architect's packet view shows the cycle's packet — never a record
/// the store holds that was not knowable at the decision — paged by
/// record, ids in full, source text after the fence note.
#[tokio::test]
async fn view_shows_only_cycle_packets() {
    let (stored, _) = strong_news();
    let bench = bench_with(&stored);
    let later = rec(
        Src::FORUM,
        "p-late",
        t("2026-10-06T09:00:00Z"),
        t("2026-10-06T09:05:00Z"),
        t("2026-10-06T09:06:00Z"),
    );
    bench.sources.add(std::slice::from_ref(&later));
    let ws = tempfile::TempDir::new().unwrap();
    let s = shared(mem(&bench), None, None, "SOE-G0", ws.path());
    let stages = ToolStages::new(
        &s,
        ws.path(),
        vec![
            ("soe_view", json!({"view": "packet"})),
            (
                "soe_view",
                json!({"view": "packet", "offset": 1, "limit": 2}),
            ),
            ("soe_view", json!({"view": "head"})),
        ],
        Vec::new(),
    );
    let p = bench.params("2026-W41", t(W41), Target::Cycle);
    let out = bench.cycle(Some(&stages), &p).await.unwrap();
    let answers = stages.answers();
    let packet = submit_mod::packet(&*bench.store, &out.dir).unwrap();
    let all = packet_page(&packet, 0, usize::MAX).0;
    assert_eq!(all.len(), 4, "{all:?}");
    let first = answers[0].2.as_ref().unwrap();
    assert!(
        first.starts_with(&format!(
            "soe_view cycles/2026-W41 packet OPEN PROPOSE decided_at={W41} shown=4 of 4 from 0 | ok"
        )),
        "{first}"
    );
    for id in &all {
        assert!(first.contains(id.as_str()), "{id} in full");
    }
    assert!(!first.contains(&later.record_id), "{first}");
    assert!(first.contains(FENCE_NOTE), "{first}");
    let row: ViewRow = serde_json::from_value(answers[0].3.as_ref().unwrap().data.clone()).unwrap();
    assert_eq!(row.ids, all);
    let second = answers[1].2.as_ref().unwrap();
    let row: ViewRow = serde_json::from_value(answers[1].3.as_ref().unwrap().data.clone()).unwrap();
    assert_eq!(row.ids, all[1..3]);
    assert!(
        second.ends_with(
            "more: 1 of 4 left — soe_view {\"run\": \"cycles/2026-W41\", \"view\": \"packet\", \"offset\": 3}"
        ),
        "{second}"
    );
    let head = answers[2].2.as_ref().unwrap();
    assert!(
        head.contains("packet ") && head.contains(": 4 records — view packet"),
        "{head}"
    );
    // A run nobody claimed.
    let e = err_of(
        call(
            &by_name(&s),
            ws.path(),
            "soe_view",
            json!({"run": "cycles/2026-W01"}),
            "v:1",
        )
        .await,
    );
    assert!(e.starts_with("run_not_found: cycles/2026-W01"), "{e}");
}

/// History (critic UNCOVERED 5): a later week sees the episodes decided
/// before it — never one decided at or after its decision, never a later
/// version of an episode; a replay of an earlier date sees none.
#[tokio::test]
async fn view_history_is_as_of_decided_at() {
    let (stored, draft) = strong_news();
    let bench = bench_with(&stored);
    let ws = tempfile::TempDir::new().unwrap();
    let s = shared(mem(&bench), Some("a"), None, "SOE-G0", ws.path());
    let w41 = ToolStages::new(
        &s,
        ws.path(),
        vec![("soe_propose", json!({ "proposal": draft }))],
        Vec::new(),
    );
    let p = bench.params("2026-W41", t(W41), Target::Cycle);
    bench.cycle(Some(&w41), &p).await.unwrap();
    let mut lines = bench.store.lines(StateLog::Episodes).unwrap();
    assert_eq!(lines.len(), 1);
    let first: OpportunityEpisode = from_json(&lines[0]).unwrap();
    assert_eq!(first.id, "2026-W41.e01");
    // A later episode (decided after W42) and a second version of W41's.
    let mut later = first.clone();
    later.id = "2026-W43.e01".into();
    later.decided_at = Time::At(t("2026-10-19T12:00:00Z"));
    let mut v2 = first.clone();
    v2.version = 2;
    for e in [&later, &v2] {
        let line = crate::domain::canonical::canonical_json(&serde_json::to_value(e).unwrap());
        bench.store.append_line(StateLog::Episodes, &line).unwrap();
        lines.push(line);
    }
    let (shown, bad) = prior_episodes(&*bench.store, t(W42)).unwrap();
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(
        shown
            .iter()
            .map(|e| (e.id.as_str(), e.version))
            .collect::<Vec<_>>(),
        [("2026-W41.e01", 1)]
    );
    let w42 = ToolStages::new(
        &s,
        ws.path(),
        vec![("soe_view", json!({"view": "history"}))],
        Vec::new(),
    );
    let p = bench.params("2026-W42", t(W42), Target::Cycle);
    bench.cycle(Some(&w42), &p).await.unwrap();
    let a = w42.answers();
    let text = a[0].2.as_ref().unwrap();
    let row: ViewRow = serde_json::from_value(a[0].3.as_ref().unwrap().data.clone()).unwrap();
    assert_eq!(row.ids, ["2026-W41.e01"], "{text}");
    assert!(
        text.contains(
            "## 2026-W41.e01 · news-automation v1 · decided 2026-10-05T12:00:00Z · verdict HOLD"
        ),
        "{text}"
    );
    assert!(!text.contains("2026-W43.e01"), "{text}");
    // At the W41 decision itself (a replay of that week): nothing before it.
    let (none, _) = prior_episodes(&*bench.store, t(W41)).unwrap();
    assert!(none.is_empty());
}

/// Writes come from a named stage agent: none named ⇒ refused before the
/// run is touched; with `[soe]`, only the Architect proposes and only the
/// Critic challenges.
#[tokio::test]
async fn stage_agent_is_named_and_in_its_role() {
    let ws = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn CycleStore> = Arc::new(MemCycleStore::default());
    let soe: SoeConfig = toml::from_str(
        "architect = \"soe_architect\"\ncritic = \"soe_critic\"\nmax_proposals = 12\nforecast_max_weeks = 12\n",
    )
    .unwrap();
    let propose = json!({"run": "cycles/2026-W41", "proposal": {"opportunity": {}}});
    let challenge = json!({"run": "cycles/2026-W41", "target": "x", "kind": "BASE_RATE",
                           "claim": "c", "effect": "NONE"});
    for (agent, tool, args, want) in [
        (None, "soe_propose", &propose, "stage_agent_unknown: soe_propose writes from a stage run"),
        (
            Some("soe_critic"),
            "soe_propose",
            &propose,
            "stage_agent_mismatch: soe_propose is the stage tool of [agents.soe_architect] ([soe]); `soe_critic` called it",
        ),
        (
            Some("soe_architect"),
            "soe_challenge",
            &challenge,
            "stage_agent_mismatch: soe_challenge is the stage tool of [agents.soe_critic] ([soe]); `soe_architect` called it",
        ),
    ] {
        let s = shared(Arc::clone(&store), agent, Some(soe.clone()), "SOE-G0", ws.path());
        let e = err_of(call(&by_name(&s), ws.path(), tool, args.clone(), "s:1").await);
        assert!(e.starts_with(want), "{e}");
    }
    // No [sources] ⇒ no state root.
    let none = SoeShared {
        store: open_store(&SandboxSections::default()),
        ..shared(Arc::clone(&store), Some("a"), None, "SOE-G0", ws.path())
    };
    let e = err_of(
        call(
            &by_name(&none),
            ws.path(),
            "soe_view",
            json!({"run": "cycles/2026-W41"}),
            "s:2",
        )
        .await,
    );
    assert!(
        e.starts_with("soe_state_missing: no [sources] section"),
        "{e}"
    );
    // A deny-all scope denies the read.
    let h = TestHarness::with_scope(ws.path(), crate::domain::scope::ToolScope::default());
    let ctx = h.ctx();
    let s = shared(store, Some("a"), None, "SOE-G0", ws.path());
    assert!(by_name(&s)["soe_view"]
        .execute(&json!({"run": "cycles/2026-W41"}), &ctx)
        .await
        .is_err());
}

/// The stamp's skill identity is the stage cache key's: a `run-agent`
/// step's exported one when set (its bridge's cwd is the workspace, where a
/// cwd-tier skill is not found), else the same function over the agent's
/// packages; a changed SKILL.md changes it.
#[test]
fn stamp_skill_identity_is_the_cache_keys() {
    use crate::adapters::outbound::soe::runner::skill_sha256;
    let ws = tempfile::TempDir::new().unwrap();
    let dir = ws.path().join(".tengu/skills/soe-stamp-test-skill");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), "# v1\n").unwrap();
    let cfg: crate::config::Config = toml::from_str(
        "[agents.a]\nengine = \"openrouter\"\nmodel = \"m\"\nskill_packages = [\"soe-stamp-test-skill\"]\n",
    )
    .unwrap();
    let agent = &cfg.agents["a"];
    let mut stamp = shared(
        Arc::new(MemCycleStore::default()),
        Some("a"),
        None,
        "SOE-G0",
        ws.path(),
    )
    .stamp;
    stamp.skill_packages = agent.skill_packages.clone();
    let key = skill_sha256(agent, ws.path());
    assert_eq!(stamp.provenance("a", Some("c"), 0).skill_sha256, key);
    std::fs::write(dir.join("SKILL.md"), "# v2\n").unwrap();
    assert_ne!(stamp.provenance("a", Some("c"), 0).skill_sha256, key);
    stamp.step_skill_sha256 = Some(key.clone());
    let p = stamp.provenance("a", None, 0);
    assert_eq!((p.skill_sha256, p.call_id.as_str()), (key, "UNKNOWN"));
}

// ---------------------------------------------------------------------------
// The fixture state root (bridge conformance, engine matrix)
// ---------------------------------------------------------------------------

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/soe")
}

/// The Architect skill's example draft (`skill_resource`), relative to
/// [`fixture_root`]: the fixture draft, kept equal by the test below.
const SKILL_EXAMPLE: &str = "../../../skills/soe-architect/resources/proposal.example.json";

/// What the fixture's stages stamp (fixed: the bytes never move).
fn fixture_stamp(agent: &str, n: usize) -> crate::domain::soe::proposal::Provenance {
    crate::domain::soe::proposal::Provenance {
        agent: agent.into(),
        model: "synthetic-model".into(),
        engine: "openrouter".into(),
        skill_sha256: skill_sha256_of(&[], Path::new("/nonexistent")),
        generation: crate::application::soe::submit::UNBOUND.into(),
        call_id: format!("fixture:{agent}:{n}"),
        proposed_at: Time::At(t("2026-10-05T12:30:00Z")),
    }
}

/// One fixture cycle on `store` (generation `UNBOUND`, the sandbox's stage
/// agent names).
async fn fixture_cycle(
    bench: &Bench,
    store: &FsCycleStore,
    runner: &dyn StageRunner,
    week: &str,
    at: &str,
    target: Target,
) {
    let mut p: CycleParams = bench.params(week, t(at), target);
    p.generation = GenerationPin {
        id: crate::application::soe::submit::UNBOUND.into(),
        sha256: FIXTURE_PIN.into(),
    };
    p.architect = "soe_architect".into();
    p.critic = "soe_critic".into();
    let env = CycleEnv {
        sources: Some(&bench.sources),
        registry: &bench.registry,
        store,
        runner: Some(runner),
        clock: &bench.clock,
        profile: ProfileIn {
            record: &bench.profile.record,
            sha256: &bench.profile.sha256,
            text: &bench.text,
        },
        owner: &Unleased,
    };
    crate::application::soe::cycle::run_cycle(&env, &p)
        .await
        .unwrap_or_else(|e| panic!("{week}: {e:#}"));
}

/// Module table: the fixture files, by path under `tests/fixtures/soe/`.
async fn fixture_files() -> BTreeMap<String, Vec<u8>> {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = FsCycleStore::new(tmp.path());
    let (stored, draft) = strong_news();
    let draft_text = draft.to_string();
    let bench = bench_with(&stored);
    let snaps: Mutex<BTreeMap<String, Vec<u8>>> = Mutex::new(BTreeMap::new());
    let snap = |req: &StageRequest| {
        let mut m = snaps.lock().unwrap();
        for f in store.files(&req.dir).unwrap() {
            let bytes = store.read(&req.dir, &f).unwrap().unwrap();
            m.insert(format!("state/{}/{f}", req.dir), bytes);
        }
    };
    let propose = |req: &StageRequest| {
        let p = fixture_stamp("soe_architect", 1);
        submit_proposal(&store, &req.dir, &draft_text, p)
            .unwrap()
            .unwrap();
    };
    // W41 live: one proposal, decided, frozen, its episode logged.
    let w41 = FnRunner(|req: &StageRequest| {
        if req.stage == Stage::Architect {
            propose(req)
        }
    });
    fixture_cycle(&bench, &store, &w41, "2026-W41", W41, Target::Cycle).await;
    let episodes = store.lines(StateLog::Episodes).unwrap();
    // W42 live, seen at its Architect stage (W41's candidate carried).
    let w42 = FnRunner(|req: &StageRequest| {
        if req.stage == Stage::Architect {
            snap(req)
        }
    });
    fixture_cycle(&bench, &store, &w42, "2026-W42", W42, Target::Cycle).await;
    // A W42 replay, seen at its Critic stage (one proposal).
    let replay = FnRunner(|req: &StageRequest| match req.stage {
        Stage::Architect => propose(req),
        _ => snap(req),
    });
    let target = Target::Replay {
        run_id: "fixture.w42".into(),
    };
    fixture_cycle(&bench, &store, &replay, "2026-W42", W42, target).await;
    let mut out = snaps.into_inner().unwrap();
    out.insert(
        "state/episodes.jsonl".into(),
        episodes
            .iter()
            .flat_map(|l| format!("{l}\n").into_bytes())
            .collect(),
    );
    let pretty = |v: &Value| format!("{}\n", serde_json::to_string_pretty(v).unwrap());
    out.insert("drafts/proposal.json".into(), pretty(&draft).into_bytes());
    out.insert(SKILL_EXAMPLE.into(), pretty(&draft).into_bytes());
    out.insert(
        "drafts/challenge.json".into(),
        pretty(&json!({
            "target": "news-automation",
            "kind": "HIDDEN_LABOR",
            "claim": "the filing format changes again: support takes more hours than the setup",
            "evidence": [],
            "effect": "WIDEN",
            "field": "economics.owner_hours_per_month",
            "high": "20",
        }))
        .into_bytes(),
    );
    out
}

fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, base, out);
        } else {
            out.push(
                p.strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
}

/// The committed fixture state is what the cycle writes now (module doc:
/// regenerate with `TENGU_REGEN_SOE_STATE=1`).
#[tokio::test(flavor = "multi_thread")]
async fn fixture_state_is_current() {
    let files = fixture_files().await;
    let root = fixture_root();
    if std::env::var_os("TENGU_REGEN_SOE_STATE").is_some() {
        for d in ["state", "drafts"] {
            let _ = std::fs::remove_dir_all(root.join(d));
        }
        for (rel, bytes) in &files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, bytes).unwrap();
        }
        return;
    }
    let mut on_disk = vec![SKILL_EXAMPLE.to_string()];
    for d in ["state", "drafts"] {
        walk(&root.join(d), &root, &mut on_disk);
    }
    on_disk.sort();
    let want: Vec<String> = files.keys().cloned().collect();
    let regen = "TENGU_REGEN_SOE_STATE=1 cargo test --bin tengu fixture_state_is_current";
    assert_eq!(on_disk, want, "fixture files differ — regenerate: {regen}");
    for (rel, bytes) in &files {
        let have = std::fs::read(root.join(rel)).unwrap();
        assert!(&have == bytes, "{rel} is stale — regenerate: {regen}");
    }
    for want in [
        "state/cycles/2026-W42/head.json",
        "state/cycles/2026-W42/phase-propose.json",
        "state/replays/fixture.w42/phase-challenge.json",
        "state/replays/fixture.w42/proposals.jsonl",
    ] {
        assert!(files.contains_key(want), "{want}");
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(e.file_name());
        if e.path().is_dir() {
            copy_tree(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), &target).unwrap();
        }
    }
}

/// The fixture state, copied to a temp root, takes every view and both
/// fixture drafts as the conformance cases send them (generation
/// `UNBOUND`, on disk).
#[tokio::test]
async fn fixture_state_takes_the_drafts() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("state");
    copy_tree(&fixture_root().join("state"), &root);
    let ws = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn CycleStore> = Arc::new(FsCycleStore::new(&root));
    let s = shared(store, Some("conf"), None, "UNBOUND", ws.path());
    let tools = by_name(&s);
    let read = |rel: &str| std::fs::read_to_string(fixture_root().join(rel)).unwrap();
    let run = "cycles/2026-W42";
    let mut texts = Vec::new();
    for view in [
        "head",
        "packet",
        "candidates",
        "proposals",
        "challenges",
        "history",
    ] {
        let text = text_of(
            call(
                &tools,
                ws.path(),
                "soe_view",
                json!({"run": run, "view": view}),
                "v",
            )
            .await,
        );
        assert!(text.len() <= TEXT_BUDGET, "{view}: {} bytes", text.len());
        texts.push(text);
    }
    assert!(
        texts[0].contains("status OPEN · phase PROPOSE"),
        "{}",
        texts[0]
    );
    assert!(
        texts[2].contains(
            "· news-automation v1 · AUTOMATE · revenue RECURRING · carried from 2026-W41"
        ),
        "{}",
        texts[2]
    );
    assert!(
        texts[5].contains("## 2026-W41.e01 · news-automation"),
        "{}",
        texts[5]
    );
    let proposal: Value = serde_json::from_str(&read("drafts/proposal.json")).unwrap();
    let ok = text_of(
        call(
            &tools,
            ws.path(),
            "soe_propose",
            json!({"run": run, "proposal": proposal.clone()}),
            "p",
        )
        .await,
    );
    assert!(
        ok.starts_with("soe_propose 2026-W42.p01 news-automation v1 AUTOMATE"),
        "{ok}"
    );
    let again = err_of(
        call(
            &tools,
            ws.path(),
            "soe_propose",
            json!({"run": run, "proposal": proposal}),
            "p",
        )
        .await,
    );
    assert!(
        again.contains("- duplicate: opportunity `news-automation`"),
        "{again}"
    );
    let mut c: Value = serde_json::from_str(&read("drafts/challenge.json")).unwrap();
    c["run"] = "replays/fixture.w42".into();
    let ok = text_of(call(&tools, ws.path(), "soe_challenge", c, "c").await);
    assert!(
        ok.starts_with("soe_challenge 2026-W42.c01 target=news-automation"),
        "{ok}"
    );
}
