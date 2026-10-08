//! `strategy_ranking` — strategy rankings of a `[strategy_ranking]` sandbox
//! (`docs/strategy-ranking-automation-2026-10-08.md` SR-4): `run` one ranking
//! date of a sealed contract through the coordinator (`application/ranking/`,
//! as `tengu ranking run` runs it), or read a published one (`latest`). No
//! network, no LLM; xlab-w2's `[feeds.strategy_ranking_*]` call it under
//! `tengu run`, an agent through any engine.
//!
//! | Step | Rule |
//! |---|---|
//! | Refuse | no `[xmarket]` ⇒ `state_dir_missing`; no `[strategy_ranking]` ⇒ `no_strategy_ranking`; a contract not listed ⇒ `contract_not_listed`; `run` only: `market.db` not openable ⇒ `market_data_unavailable`, and the coordinator's own refusals (`contract_unsealed`, `contract_changed`, `ranking_busy`, `not_a_ranking_day`, `cutoff_not_reached`, `before_from`, …) |
//! | Args (strict) | `action` (`run` \| `latest`, required); `contract` (default: the only one listed); `date` (`YYYY-MM-DD`, the contract's zone). An unknown key or a wrong type is an error: nothing runs |
//! | Run | `run_ranking` on a blocking thread with its own runtime (the backtests are CPU: the caller's runtime keeps serving); lease holder = the call id (`feed:…`, `mcp:…`, `chat:…`), else `tool:<pid>`. A published date comes back as it is (nothing reruns) |
//! | Latest | `latest.json` — or with `date`, that date's `ranking.json` once its manifest is published: no lease, no `market.db` read, nothing written |
//! | INCOMPLETE | an error (`ranking_incomplete: …`) carrying the whole text — as `tengu ranking run` exits 1: a feed shows it `down`, `tengu doctor --live` names it; `latest` keeps the previous COMPLETE ranking |
//! | Text | [`render`]: line 1 = action, contract, date, status, what the call did, the counts; then `Ranking::render_compact` (rows weakest → strongest per cohort, ids and run locators whole); how to read a row's run (`backtest` `run_id`); the files relative to the state dir — never its path (outside every fs root: the operator's). ≤ [`TEXT_MAX_CHARS`] for 15 strategies in 15 cohorts (the cap test below) |
//! | Row | none (`observation` `None`): the ranking is its files |

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use chrono::NaiveDate;
use serde_json::Value;
use tracing::info;

use super::{defs, object_args, opt_str, XlabShared};
use crate::adapters::outbound::clock::SystemClock;
use crate::adapters::outbound::runtime_store::SqliteRuntimeStore;
use crate::adapters::outbound::tools::xm::STATE_DIR_MISSING;
use crate::application::ranking::store::published_ranking;
use crate::application::ranking::{
    pick_contract, ranking_section, run_ranking, RankOutcome, RankRequest, RankingEnv,
};
use crate::config::xmarket::RANKINGS_DIR;
use crate::domain::backtest::ranking::{Ranking, RankingStatus};
use crate::domain::message::ToolDef;
use crate::domain::tools as names;
use crate::ports::tool::{Tool, ToolCtx, ToolOutput};

/// The text's budget: 15 strategies, each its own cohort, the longest
/// xlab-w2 names (the cap test: ≈ 3.6 KB; one cohort of 15 ≈ 2.7 KB) —
/// whole under a 16k local model's 8 192-char cap.
#[cfg(test)]
pub(crate) const TEXT_MAX_CHARS: usize = 4_096;

const ARGS: &[&str] = &["action", "contract", "date"];

pub(crate) fn tools(shared: &XlabShared) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(RankTool {
        def: defs::def(names::STRATEGY_RANKING),
        shared: shared.clone(),
    })]
}

pub(crate) struct RankTool {
    def: ToolDef,
    shared: XlabShared,
}

/// What a call asks for (module table).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Request {
    pub action: Action,
    pub contract: Option<String>,
    pub date: Option<NaiveDate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Run,
    Latest,
}

#[async_trait]
impl Tool for RankTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        ctx.scope.check_fs_write(ctx.workspace)?;
        let tool = names::STRATEGY_RANKING;
        let sections = Arc::clone(&self.shared.sandbox);
        let Some(state_dir) = sections.xm_state_dir.clone() else {
            bail!(
                "{STATE_DIR_MISSING}: strategy rankings unavailable: no [xmarket] section — add \
                 [xmarket] state = \"<name>\" (the rankings live in \
                 <TENGU_HOME>/state/<name>/{RANKINGS_DIR}/)"
            );
        };
        let section = ranking_section(&sections)?;
        let req = parse_args(args)?;
        let contract = pick_contract(section, req.contract.as_deref())?;
        let state = state_name(&state_dir);
        if req.action == Action::Latest {
            let dir = state_dir.clone();
            let id = contract.clone();
            let read = tokio::task::spawn_blocking(move || published_ranking(&dir, &id, req.date))
                .await
                .map_err(|e| anyhow!("{tool}: the read stopped: {e}"))??;
            let Some(ranking) = read else {
                bail!(
                    "no published ranking of `{contract}` {} in state `{state}` — action run \
                     publishes one (latest moves on COMPLETE only; a RUNNING or FAILED date \
                     publishes nothing)",
                    req.date.map_or("yet".to_string(), |d| format!("for {d}"))
                );
            };
            let did = Did::Read {
                dated: req.date.is_some(),
            };
            return Ok(ToolOutput {
                text: render(&ranking, &state, did),
                observation: None,
            });
        }
        let env = RankingEnv {
            store: self.shared.market_arc()?,
            sections,
            runtime: Arc::new(SqliteRuntimeStore::open(&state_dir)?),
            clock: Arc::new(SystemClock),
            state_dir,
        };
        let request = RankRequest {
            contract,
            date: req.date,
            holder: ctx
                .call_id
                .map_or_else(|| format!("tool:{}", std::process::id()), str::to_string),
        };
        let out = run_blocking(env, request).await?;
        // The path is the operator's (logs), never the model's text.
        info!(
            contract = %out.ranking.contract, date = %out.ranking.date,
            status = ?out.status, published = out.published, dir = %out.dir.display(),
            "strategy_ranking tool: ranking date done"
        );
        let did = Did::Ran {
            now: out.published,
            latest_replaced: out.latest_replaced,
        };
        let text = render(&out.ranking, &state, did);
        if out.status == RankingStatus::Incomplete {
            bail!(
                "ranking_incomplete: `{}` {} is INCOMPLETE — {} listed strateg(ies) without a \
                 run; the dated files are written, latest is kept\n{text}",
                out.ranking.contract,
                out.ranking.date,
                out.ranking.failed.len()
            );
        }
        Ok(ToolOutput {
            text,
            observation: None,
        })
    }
}

/// `run_ranking` on a blocking thread with a runtime of its own (module
/// table): the backtests' bootstrap is CPU-bound.
async fn run_blocking(env: RankingEnv, req: RankRequest) -> Result<RankOutcome> {
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(run_ranking(&env, req))
    })
    .await
    .map_err(|e| anyhow!("{}: the ranking stopped: {e}", names::STRATEGY_RANKING))?
}

/// The request `args` make (module table).
pub(crate) fn parse_args(args: &Value) -> Result<Request> {
    let tool = names::STRATEGY_RANKING;
    let o = object_args(tool, args, ARGS)?;
    let action = match opt_str(tool, o, "action")? {
        Some("run") => Action::Run,
        Some("latest") => Action::Latest,
        Some(a) => bail!("{tool}: 'action' `{a}` — run or latest"),
        None => bail!("{tool}: 'action' is required — run or latest"),
    };
    let contract = opt_str(tool, o, "contract")?.map(str::to_string);
    let date = opt_str(tool, o, "date")?
        .map(|s| {
            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|_| anyhow!("{tool}: 'date' `{s}` is not a date (YYYY-MM-DD)"))
        })
        .transpose()?;
    Ok(Request {
        action,
        contract,
        date,
    })
}

/// The state dir's name: the `<state>` of the rows' `run:<state>/<run id>`.
fn state_name(state_dir: &Path) -> String {
    state_dir
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// What the call did, for line 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Did {
    /// `run`: `now` = ran and published by this call (else an earlier run's).
    Ran { now: bool, latest_replaced: bool },
    /// `latest`: `dated` = a date's ranking, else latest.
    Read { dated: bool },
}

/// The tool's text (module table).
fn render(r: &Ranking, state: &str, did: Did) -> String {
    let ranked: usize = r.cohorts.iter().map(|c| c.rows.len()).sum();
    let (action, what) = match did {
        Did::Ran {
            now: true,
            latest_replaced,
        } => (
            "run",
            if latest_replaced {
                "ran now · latest replaced"
            } else {
                "ran now · latest kept"
            },
        ),
        Did::Ran { now: false, .. } => ("run", "already published by an earlier run — nothing ran"),
        Did::Read { dated: false } => ("latest", "the newest COMPLETE ranking — nothing ran"),
        Did::Read { dated: true } => ("latest", "that date's published ranking — nothing ran"),
    };
    let status = match r.status {
        RankingStatus::Complete => "COMPLETE",
        RankingStatus::Incomplete => "INCOMPLETE",
    };
    let mut s = format!(
        "strategy_ranking {action} {} {} {status} · {what} · {ranked} ranked, {} ineligible, {} \
         failed, {} dropped\n",
        r.contract,
        r.date,
        r.ineligible.len(),
        r.failed.len(),
        r.dropped.len()
    );
    s.push_str(&r.render_compact());
    s.push_str(
        "rows: run:<state>/<run id> — read a run with backtest {\"run_id\": \"<run id>\", \"view\": \
         \"periods\"}\n",
    );
    s.push_str(&format!(
        "files: {RANKINGS_DIR}/{}/{}/ranking.json + .md, {RANKINGS_DIR}/{}/latest.json + .md in \
         state dir `{state}` — outside your workspace",
        r.contract, r.date, r.contract
    ));
    s
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::adapters::outbound::tools::workspace::test_support::TestHarness;
    use crate::application::ranking::store::{
        contract_dir, date_dir, read_manifest, write_manifest, ManifestStatus, LATEST_JSON,
    };
    use crate::application::ranking::tests::seed;
    use crate::config::sections::SandboxSections;
    use crate::config::strategy_ranking::tests::setup;
    use crate::config::xmarket::backtests_dir;
    use crate::config::Config;
    use crate::domain::scope::ToolScope;
    use crate::ports::market_data::MarketDataStore;

    const AAPL: &str = "hyperliquid:xyz:AAPL";
    const TSLA: &str = "hyperliquid:xyz:TSLA";
    /// The lineage fixture's sealed contract (`ranked`: rule_w, rule_w_top4).
    const ID: &str = "rank.fixture.v1";
    /// Seeded bars end here: every date through 2026-10-07 is past its
    /// cutoff (00:00 New York) on the wall clock the tool reads.
    const END: &str = "2026-10-08 00:00";
    const DATE: &str = "2026-10-07";

    /// The ranked sandbox of `config::strategy_ranking::tests` with its
    /// state dir at `<tmp>/state/ranked`.
    fn ranked() -> (tempfile::TempDir, PathBuf, Arc<SandboxSections>) {
        let (tmp, path) = setup("ranked", |t| t);
        let cfg = Config::load(&path).unwrap_or_else(|e| panic!("{e:#}"));
        let mut s = (*cfg.agents["architect"].sandbox).clone();
        let state = tmp.path().join("state/ranked");
        s.xm_state_dir = Some(state.clone());
        (tmp, state, Arc::new(s))
    }

    fn tool(market: Result<Arc<dyn MarketDataStore>, String>, s: Arc<SandboxSections>) -> RankTool {
        RankTool {
            def: defs::def(names::STRATEGY_RANKING),
            shared: XlabShared {
                market,
                store: None,
                sandbox: s,
            },
        }
    }

    async fn err(t: &RankTool, h: &TestHarness, args: Value) -> String {
        format!("{:#}", t.execute(&args, &h.ctx()).await.unwrap_err())
    }

    #[test]
    fn arguments_parse_strictly() {
        let r = parse_args(&json!({"action": "run", "contract": "rank.x", "date": "2026-10-07"}))
            .unwrap();
        assert_eq!(
            r,
            Request {
                action: Action::Run,
                contract: Some("rank.x".into()),
                date: NaiveDate::from_ymd_opt(2026, 10, 7),
            }
        );
        let r = parse_args(&json!({"action": "latest", "contract": null})).unwrap();
        assert_eq!((r.action, r.contract, r.date), (Action::Latest, None, None));
        for (args, needle) in [
            (json!({}), "'action' is required"),
            (json!({"action": "rank"}), "'action' `rank` — run or latest"),
            (
                json!({"action": "run", "date": "10/07/2026"}),
                "is not a date (YYYY-MM-DD)",
            ),
            (
                json!({"action": "run", "date": 20261007}),
                "'date' must be a non-empty string",
            ),
            (
                json!({"action": "run", "split": "time:2026-07-01"}),
                "unknown argument(s) [\"split\"]",
            ),
            (json!("run"), "arguments must be a JSON object"),
        ] {
            let e = parse_args(&args).unwrap_err().to_string();
            assert!(e.contains(needle), "{args}: {e}");
        }
    }

    /// The scope check comes first; then each missing section is named by
    /// its refusal code — nothing runs, nothing is written.
    #[tokio::test]
    async fn execute_gates_scope_and_refuses_without_its_sections() {
        let (tmp, state, sections) = ranked();
        let run = json!({"action": "run", "date": DATE});
        // Scope: a workspace outside fs_roots.
        let denied = TestHarness::with_scope(tmp.path(), ToolScope::default());
        let t = tool(Err("unused".into()), Arc::clone(&sections));
        let e = err(&t, &denied, run.clone()).await;
        assert!(e.starts_with("fs write denied for "), "{e}");
        let h = TestHarness::new(tmp.path());
        // No [xmarket]: the family's refusal.
        let t = tool(Err("unused".into()), Arc::new(SandboxSections::default()));
        let e = err(&t, &h, run.clone()).await;
        assert!(
            e.starts_with("state_dir_missing: strategy rankings unavailable: no [xmarket]"),
            "{e}"
        );
        // No [strategy_ranking].
        let mut s = (*sections).clone();
        s.ranking = None;
        let t = tool(Err("unused".into()), Arc::new(s));
        let e = err(&t, &h, run.clone()).await;
        assert!(e.starts_with("no_strategy_ranking: "), "{e}");
        // A contract not listed; bad arguments.
        let t = tool(
            Err("market_data_unavailable: no store".into()),
            Arc::clone(&sections),
        );
        let e = err(&t, &h, json!({"action": "run", "contract": "rank.nope"})).await;
        assert!(e.starts_with("contract_not_listed: `rank.nope`"), "{e}");
        let e = err(&t, &h, json!({"action": "rank"})).await;
        assert!(e.contains("run or latest"), "{e}");
        // A run needs the warehouse: its refusal, before any lease.
        let e = err(&t, &h, run).await;
        assert!(e.starts_with("market_data_unavailable: "), "{e}");
        assert!(!state.exists(), "nothing written: {}", state.display());
    }

    /// `latest` reads the published files only: no warehouse, no lease, no
    /// run — and says so when nothing is published.
    #[tokio::test]
    async fn latest_reads_without_running() {
        let (tmp, state, sections) = ranked();
        let h = TestHarness::new(tmp.path());
        let unavailable = || Err("market_data_unavailable: no store".to_string());
        let t = tool(unavailable(), Arc::clone(&sections));
        let e = err(&t, &h, json!({"action": "latest"})).await;
        assert!(
            e.starts_with("no published ranking of `rank.fixture.v1` yet in state `ranked`"),
            "{e}"
        );
        // Publish one through the tool (a warehouse), then read it without.
        let store = seed(&state, [(AAPL, END), (TSLA, END)]).await;
        let ran = tool(Ok(store), Arc::clone(&sections))
            .execute(&json!({"action": "run", "date": DATE}), &h.ctx())
            .await
            .unwrap();
        let runs = std::fs::read_dir(backtests_dir(&state)).unwrap().count();
        // The run's lease store goes: a read never opens it.
        for f in ["runtime.db", "runtime.db-wal", "runtime.db-shm"] {
            let _ = std::fs::remove_file(state.join(f));
        }
        let out = t
            .execute(&json!({"action": "latest"}), &h.ctx())
            .await
            .unwrap();
        assert!(out.observation.is_none());
        let line1 = out.text.lines().next().unwrap();
        assert!(
            line1.starts_with(&format!(
                "strategy_ranking latest {ID} {DATE} COMPLETE · the newest COMPLETE ranking — \
                 nothing ran · 2 ranked"
            )),
            "{}",
            out.text
        );
        // The same ranking as the run's, after line 1.
        let tail = |s: &str| s.split_once('\n').unwrap().1.to_string();
        assert_eq!(tail(&out.text), tail(&ran.text));
        let dated = t
            .execute(&json!({"action": "latest", "date": DATE}), &h.ctx())
            .await
            .unwrap();
        assert!(
            dated.text.contains("that date's published ranking"),
            "{}",
            dated.text
        );
        assert_eq!(
            std::fs::read_dir(backtests_dir(&state)).unwrap().count(),
            runs
        );
        assert!(!state.join("runtime.db").exists(), "no lease taken");
        // A date still RUNNING publishes nothing.
        let dir = date_dir(
            &state,
            ID,
            NaiveDate::parse_from_str(DATE, "%Y-%m-%d").unwrap(),
        );
        let mut m = read_manifest(&dir).unwrap().unwrap();
        m.status = ManifestStatus::Running;
        write_manifest(&dir, &m).unwrap();
        let e = err(&t, &h, json!({"action": "latest", "date": DATE})).await;
        assert!(e.contains("for 2026-10-07 in state `ranked`"), "{e}");
    }

    /// `run` publishes through the coordinator (holder = the call id), then
    /// returns the published date as it is; the text names rows and run
    /// locators whole, the files relative to the state dir, never its path.
    #[tokio::test]
    async fn run_publishes_then_returns_the_published_date() {
        let (tmp, state, sections) = ranked();
        let store = seed(&state, [(AAPL, END), (TSLA, END)]).await;
        let t = tool(Ok(store), sections);
        let h = TestHarness::new(tmp.path());
        let args = json!({"action": "run", "date": DATE});
        let first = t.execute(&args, &h.ctx()).await.unwrap();
        let text = &first.text;
        assert!(
            text.starts_with(&format!(
                "strategy_ranking run {ID} {DATE} COMPLETE · ran now · latest replaced · 2 ranked, \
                 0 ineligible, 0 failed, 0 dropped\nstrategy ranking `{ID}` {DATE}: COMPLETE · \
                 contract sha256 "
            )),
            "{text}"
        );
        for strategy in ["rule_w", "rule_w_top4"] {
            assert!(
                regex::Regex::new(&format!(
                    r"\n  1\. {strategy}  ci95_lo \S+  mean \S+  n \d+  run:ranked/\d{{8}}T\d{{6}}Z-{strategy}\n"
                ))
                .unwrap()
                .is_match(text),
                "{text}"
            );
        }
        assert!(
            text.ends_with(&format!(
                "files: strategy-rankings/{ID}/{DATE}/ranking.json + .md, \
                 strategy-rankings/{ID}/latest.json + .md in state dir `ranked` — outside your \
                 workspace"
            )),
            "{text}"
        );
        assert!(!text.contains(&state.display().to_string()), "{text}");
        let m = read_manifest(&date_dir(
            &state,
            ID,
            NaiveDate::parse_from_str(DATE, "%Y-%m-%d").unwrap(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(m.status, ManifestStatus::Complete);
        assert_eq!(m.holder, format!("tool:{}", std::process::id()));
        assert!(contract_dir(&state, ID).join(LATEST_JSON).is_file());
        let again = t.execute(&args, &h.ctx()).await.unwrap();
        assert!(
            again.text.starts_with(&format!(
                "strategy_ranking run {ID} {DATE} COMPLETE · already published by an earlier run \
                 — nothing ran"
            )),
            "{}",
            again.text
        );
        assert_eq!(std::fs::read_dir(backtests_dir(&state)).unwrap().count(), 2);
    }

    /// INCOMPLETE is an error carrying the ranking: TSLA's bars stop a week
    /// early, so rule W (AAPL + TSLA) is STALE; latest is never written.
    #[tokio::test]
    async fn an_incomplete_ranking_is_an_error_with_its_rows() {
        let (tmp, state, sections) = ranked();
        let store = seed(&state, [(AAPL, END), (TSLA, "2026-10-01 00:00")]).await;
        let t = tool(Ok(store), sections);
        let h = TestHarness::new(tmp.path());
        let e = err(&t, &h, json!({"action": "run", "date": DATE})).await;
        assert!(
            e.starts_with(&format!(
                "ranking_incomplete: `{ID}` {DATE} is INCOMPLETE — 1 listed strateg(ies) without \
                 a run; the dated files are written, latest is kept\nstrategy_ranking run {ID} \
                 {DATE} INCOMPLETE · ran now · latest kept · 1 ranked"
            )),
            "{e}"
        );
        assert!(e.contains("\nfailed (1): rule_w stale"), "{e}");
        assert!(!contract_dir(&state, ID).join(LATEST_JSON).exists());
    }

    /// The text's budget (module table): 15 strategies with the longest
    /// xlab-w2 names, each a cohort of its own, 15-row failed / ineligible
    /// lists can't add more (a strategy sits in one list).
    #[tokio::test]
    async fn the_text_fits_its_budget() {
        let (tmp, state, sections) = ranked();
        let store = seed(&state, [(AAPL, END), (TSLA, END)]).await;
        let t = tool(Ok(store), sections);
        let h = TestHarness::new(tmp.path());
        t.execute(&json!({"action": "run", "date": DATE}), &h.ctx())
            .await
            .unwrap();
        let mut r = published_ranking(&state, ID, None).unwrap().unwrap();
        let cohort = r.cohorts[0].clone();
        let long = "weekend_fade_top4_stop500_net_of_cost";
        r.cohorts = (0..15)
            .map(|i| {
                let mut c = cohort.clone();
                let row = &mut c.rows[0];
                row.strategy = format!("{long}_{i:02}");
                row.run = format!("run:xlab/20261009T050000Z-{long}_{i:02}");
                row.ci95_bps = Some([-1234.56, 1234.56]);
                row.mean_net_bps = Some(-1234.56);
                row.n = 12_345;
                c
            })
            .collect();
        let text = render(
            &r,
            "xlab",
            Did::Ran {
                now: true,
                latest_replaced: true,
            },
        );
        assert!(
            text.len() <= TEXT_MAX_CHARS,
            "{} chars:\n{text}",
            text.len()
        );
        assert_eq!(text.lines().filter(|l| l.contains(long)).count(), 15);
    }
}
