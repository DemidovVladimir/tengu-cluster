//! `CachedStageRunner` — a replay-deterministic `ports::soe::StageRunner`:
//! each stage run is recorded under `<state root>/stage-cache/<key>.jsonl`,
//! so a rerun asks no model and writes the same records (the decision
//! cache's rules, `outbound/decision_cache.rs`).
//!
//! | Rule | How |
//! |---|---|
//! | Key ([`stage_key`]) | sha256, 64 lowercase hex chars (never shortened), of the canonical JSON `{agent, model, skill_sha256, stage, goal}` (`domain/canonical.rs`); `model` and `skill_sha256` are the agent's ([`StageIdent`]: its `model`, `runner::skill_sha256`); `goal` without its `run:` line — that names where the tools write, not what the stage is asked, so a replay of a week asks what its cycle asked |
//! | Hit | every recorded record re-submitted, in order, through the tools' path (`application::soe::submit::{submit_proposal, submit_challenge}`) into the request's run dir: the same draft and provenance stamp the same record; the recorded reply returned verbatim (its metrics are the original run's) |
//! | Hit refused | a recorded record the run dir refuses: `Err` (`stage_cache_refused`, the key in full + the problems) — the cycle records a failed stage |
//! | Miss, online | the inner runner; then the records this stage appended (proposals for `ARCHITECT`, challenges for `CHALLENGE`, past the count before the run) and the reply are written once (`create_new`; a concurrent identical miss keeps the first file) |
//! | Miss, offline (`inner = None`) | `Err` naming the key in full and the file (`stage_cache_miss`) |
//! | Inner `Err` | returned; nothing recorded (the next run asks again) |
//! | Unknown agent | `Err` (`stage_cache_agent`): no identity, no key |
//! | File | line 1 the header `{schema = "soe.stage_cache/1", key, request, reply}`, then one `{draft, provenance}` per record; dir 0700, file 0600, one `write_all`, never rewritten |

use std::collections::BTreeMap;
use std::fs::{DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::info;

use crate::application::soe::submit::{submit_challenge, submit_proposal};
use crate::domain::canonical::{canonical_json, canonical_sha256};
use crate::domain::metrics::MetricsRecord;
use crate::domain::soe::ops::Stage;
use crate::domain::soe::proposal::Provenance;
use crate::domain::soe::value::ValueError;
use crate::ports::soe::{CycleStore, StageReply, StageRequest, StageRunner};

/// The cache dir under the SOE state root.
pub(crate) const STAGE_CACHE: &str = "stage-cache";
/// The header's schema.
const SCHEMA: &str = "soe.stage_cache/1";

/// What a stage's agent is, beside its name (module table: Key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageIdent {
    pub model: String,
    pub skill_sha256: String,
}

/// The reply as recorded (module table: File).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedReply {
    ok: bool,
    summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    latency_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    metrics: Vec<MetricsRecord>,
}

impl RecordedReply {
    fn of(r: &StageReply) -> Self {
        Self {
            ok: r.ok,
            summary: r.summary.clone(),
            error: r.error.clone(),
            latency_ms: r.latency_ms,
            metrics: r.metrics.clone(),
        }
    }

    fn reply(self) -> StageReply {
        StageReply {
            ok: self.ok,
            summary: self.summary,
            error: self.error,
            latency_ms: self.latency_ms,
            metrics: self.metrics,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    schema: String,
    key: String,
    request: Value,
    reply: RecordedReply,
}

/// One record a stage appended: the model's draft and the tool's stamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recorded {
    draft: Value,
    provenance: Provenance,
}

/// Module table: Key — the key and the canonical request it hashes.
pub(crate) fn stage_key(req: &StageRequest, ident: &StageIdent) -> (String, Value) {
    let goal: Vec<&str> = req
        .goal
        .lines()
        .filter(|l| !l.starts_with("run:"))
        .collect();
    let request = json!({
        "agent": req.agent,
        "model": ident.model,
        "skill_sha256": ident.skill_sha256,
        "stage": req.stage,
        "goal": goal.join("\n"),
    });
    (canonical_sha256(&request), request)
}

/// The record-replaying stage runner (module table).
pub(crate) struct CachedStageRunner {
    dir: PathBuf,
    store: Arc<dyn CycleStore>,
    /// Answers misses; `None` = offline (a miss is an error).
    inner: Option<Arc<dyn StageRunner>>,
    idents: BTreeMap<String, StageIdent>,
}

impl CachedStageRunner {
    /// The cache at `<state_root>/stage-cache/` over `store` (the run dirs
    /// the stages write); `idents` = every stage agent's identity.
    pub(crate) fn new(
        state_root: &Path,
        store: Arc<dyn CycleStore>,
        inner: Option<Arc<dyn StageRunner>>,
        idents: BTreeMap<String, StageIdent>,
    ) -> Self {
        Self {
            dir: state_root.join(STAGE_CACHE),
            store,
            inner,
            idents,
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.jsonl"))
    }

    /// The records of `req`'s kind in its run dir, as `{draft, provenance}`.
    fn records(&self, req: &StageRequest) -> Result<Vec<Recorded>> {
        let to = |draft: Result<Value, serde_json::Error>, provenance: &Provenance| {
            Ok(Recorded {
                draft: draft?,
                provenance: provenance.clone(),
            })
        };
        match req.stage {
            Stage::Architect => self
                .store
                .proposals(&req.dir)?
                .iter()
                .map(|p| to(serde_json::to_value(&p.draft), &p.provenance))
                .collect(),
            Stage::Challenge => self
                .store
                .challenges(&req.dir)?
                .iter()
                .map(|c| to(serde_json::to_value(&c.draft), &c.provenance))
                .collect(),
            other => bail!("stage {other:?} writes no record"),
        }
    }

    /// Module table: Hit.
    fn replay(&self, req: &StageRequest, key: &str, text: &str) -> Result<StageReply> {
        let path = self.path(key);
        let mut lines = text.lines();
        let header: Header = serde_json::from_str(lines.next().unwrap_or_default())
            .with_context(|| format!("{}: line 1 is no stage-cache header", path.display()))?;
        if header.schema != SCHEMA || header.key != key {
            bail!(
                "{}: header schema `{}` key `{}` — want `{SCHEMA}` and `{key}`",
                path.display(),
                header.schema,
                header.key
            );
        }
        for (i, line) in lines.enumerate() {
            let r: Recorded = serde_json::from_str(line)
                .with_context(|| format!("{} line {}", path.display(), i + 2))?;
            let draft = canonical_json(&r.draft);
            let refused: Option<Vec<ValueError>> = match req.stage {
                Stage::Architect => {
                    submit_proposal(&*self.store, &req.dir, &draft, r.provenance)?.err()
                }
                _ => submit_challenge(&*self.store, &req.dir, &draft, r.provenance)?.err(),
            };
            if let Some(e) = refused {
                bail!(
                    "stage_cache_refused: {key}: record {} of {} is refused in {}: {}",
                    i + 1,
                    path.display(),
                    req.dir,
                    e.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                );
            }
        }
        Ok(header.reply.reply())
    }

    /// Module table: File — written once.
    fn record(
        &self,
        key: &str,
        request: Value,
        reply: &StageReply,
        new: &[Recorded],
    ) -> Result<()> {
        let mut text = canonical_json(&serde_json::to_value(Header {
            schema: SCHEMA.into(),
            key: key.into(),
            request,
            reply: RecordedReply::of(reply),
        })?);
        text.push('\n');
        for r in new {
            text.push_str(&canonical_json(&serde_json::to_value(r)?));
            text.push('\n');
        }
        let mut b = DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(&self.dir)
            .with_context(|| format!("create {}", self.dir.display()))?;
        let path = self.path(key);
        let mut o = OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut f = match o.open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
        };
        f.write_all(text.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
        f.sync_all()
            .with_context(|| format!("sync {}", path.display()))
    }
}

#[async_trait]
impl StageRunner for CachedStageRunner {
    async fn run(&self, req: &StageRequest) -> Result<StageReply> {
        let ident = self.idents.get(&req.agent).ok_or_else(|| {
            anyhow!(
                "stage_cache_agent: no identity for agent `{}` — not a stage agent of this sandbox",
                req.agent
            )
        })?;
        let (key, request) = stage_key(req, ident);
        let path = self.path(&key);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                info!(agent = %req.agent, %key, "soe stage cache hit");
                return self.replay(req, &key, &text);
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        }
        let Some(inner) = &self.inner else {
            bail!(
                "stage_cache_miss: {key}: no recorded stage {:?} of agent `{}` in {} — run it \
                 online first",
                req.stage,
                req.agent,
                path.display()
            );
        };
        let before = self.records(req)?.len();
        let reply = inner.run(req).await?;
        let all = self.records(req)?;
        let new = all.get(before..).unwrap_or_default();
        self.record(&key, request, &reply, new)?;
        info!(agent = %req.agent, %key, records = new.len(), "soe stage recorded");
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::soe::store::tests::Root;
    use crate::adapters::outbound::soe::store::FsCycleStore;
    use crate::application::soe::cycle::Target;
    use crate::application::soe::freeze::decision_sha256;
    use crate::application::soe::tests::{
        build_records, chain_specs, drafts, load_case, Bench, ScriptedRunner,
    };
    use crate::domain::source::SourceRecord;
    use crate::ports::soe::{RunDir, CHALLENGES, PROPOSALS};

    fn idents() -> BTreeMap<String, StageIdent> {
        ["soe-architect", "soe-critic"]
            .into_iter()
            .map(|a| {
                (
                    a.to_string(),
                    StageIdent {
                        model: "synthetic-model".into(),
                        skill_sha256: "ab".repeat(32),
                    },
                )
            })
            .collect()
    }

    fn request(run: &str, goal_tail: &str) -> StageRequest {
        StageRequest {
            stage: Stage::Architect,
            agent: "soe-architect".into(),
            dir: RunDir::parse(run).unwrap(),
            cycle_id: "2026-W41".into(),
            goal: format!("soe_stage: ARCHITECT\nrun: {run}\ncycle: 2026-W41\n{goal_tail}"),
        }
    }

    /// An offline miss names the key in full and the file; the key ignores
    /// where the stage writes, never what it is asked or who answers.
    #[tokio::test]
    async fn offline_miss_names_full_key() {
        let root = Root(tempfile::TempDir::new().unwrap());
        let store: Arc<dyn CycleStore> = Arc::new(FsCycleStore::new(root.0.path()));
        let cache = CachedStageRunner::new(root.0.path(), store, None, idents());
        let req = request("cycles/2026-W41", "packet_sha256: aa\n");
        let (key, _) = stage_key(&req, &idents()["soe-architect"]);
        assert_eq!(key.len(), 64);
        let e = cache.run(&req).await.unwrap_err().to_string();
        assert!(e.starts_with(&format!("stage_cache_miss: {key}: ")), "{e}");
        let file = root.0.path().join(STAGE_CACHE).join(format!("{key}.jsonl"));
        assert!(e.contains(&file.display().to_string()), "{e}");
        assert!(!file.exists() && !root.0.path().join(STAGE_CACHE).exists());

        // Same question from another run dir: the same key.
        let replay = request("replays/r-1", "packet_sha256: aa\n");
        assert_eq!(stage_key(&replay, &idents()["soe-architect"]).0, key);
        // Another question, model, skill or stage: another key.
        let ident = &idents()["soe-architect"];
        let other_goal = request("cycles/2026-W41", "packet_sha256: bb\n");
        let mut other_model = ident.clone();
        other_model.model = "another-model".into();
        let mut other_skill = ident.clone();
        other_skill.skill_sha256 = "cd".repeat(32);
        let mut critic = req.clone();
        critic.stage = Stage::Challenge;
        for k in [
            stage_key(&other_goal, ident).0,
            stage_key(&req, &other_model).0,
            stage_key(&req, &other_skill).0,
            stage_key(&critic, ident).0,
        ] {
            assert_ne!(k, key);
        }
        // No identity: no key.
        let mut stranger = req.clone();
        stranger.agent = "stranger".into();
        let e = cache.run(&stranger).await.unwrap_err().to_string();
        assert!(e.starts_with("stage_cache_agent: "), "{e}");
    }

    /// Run `case` live in `bench` through `runner` (the bench's `try_case`
    /// with the runner given).
    async fn run_live(
        bench: &Bench,
        case_id: &str,
        runner: &dyn StageRunner,
    ) -> crate::application::soe::cycle::CycleOutcome {
        let case = load_case(case_id);
        let (by, _) = build_records(&chain_specs(&case));
        let stored: Vec<SourceRecord> = case
            .records
            .iter()
            .filter(|r| r.stored)
            .map(|r| by[&r.native].clone())
            .collect();
        bench.sources.add(&stored);
        let decided = case.decided_at.earliest().unwrap();
        let mut p = bench.params(&case.week, decided, Target::Cycle);
        p.active = case.active.clone();
        bench.cycle(Some(runner), &p).await.unwrap()
    }

    /// A cycle recorded online reruns offline in a fresh state with the
    /// same proposal and challenge lines, the same stage replies and the
    /// same decision bytes — no model asked.
    #[tokio::test]
    async fn hit_is_verbatim() {
        let cache_root = Root(tempfile::TempDir::new().unwrap());
        let case = load_case("platform_rule_change");
        let (by, _) = build_records(&chain_specs(&case));

        let live = Bench::new();
        let scripted = Arc::new(ScriptedRunner::new(
            live.store.clone(),
            drafts(&case.proposals, &by),
            drafts(&case.challenges, &by),
        ));
        let online = CachedStageRunner::new(
            cache_root.0.path(),
            live.store.clone() as Arc<dyn CycleStore>,
            Some(scripted.clone() as Arc<dyn StageRunner>),
            idents(),
        );
        let first = run_live(&live, "platform_rule_change", &online).await;
        assert_eq!(scripted.requests.lock().unwrap().len(), 2);
        let recorded: Vec<_> = std::fs::read_dir(cache_root.0.path().join(STAGE_CACHE))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(recorded.len(), 2, "one file per stage: {recorded:?}");
        let architect = recorded
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .find(|t| t.contains("\"stage\":\"ARCHITECT\""))
            .unwrap();
        assert_eq!(architect.lines().count(), 1 + case.proposals.len());

        let rerun = Bench::new();
        let offline = CachedStageRunner::new(
            cache_root.0.path(),
            rerun.store.clone() as Arc<dyn CycleStore>,
            None,
            idents(),
        );
        let second = run_live(&rerun, "platform_rule_change", &offline).await;
        let dir = RunDir::Cycle(first.cycle_id.clone());
        let (a, b) = (live.store.dir(&dir), rerun.store.dir(&dir));
        for name in [PROPOSALS, CHALLENGES, "stages.json"] {
            assert!(a.contains_key(name), "{name}");
            assert_eq!(a.get(name), b.get(name), "{name}");
        }
        assert_eq!(first.stages, second.stages, "the same replies and metrics");
        assert_eq!(
            decision_sha256(&*live.store, &dir).unwrap(),
            decision_sha256(&*rerun.store, &dir).unwrap()
        );
        assert_eq!(first.decision_sha256, second.decision_sha256);
        // The recorded files are kept as they were.
        for p in &recorded {
            assert!(p.is_file());
        }
    }
}
