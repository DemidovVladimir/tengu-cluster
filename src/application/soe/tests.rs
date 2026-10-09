//! Cycle tests (O3 exit; roadmap § 13 invariants) on in-memory fakes of the
//! ports and the synthetic cycle cases `tests/fixtures/soe/cycles/<id>.toml`
//! (`soe.cycle_case/1`, synthetic profile `synthetic-operator`).
//!
//! | `soe.cycle_case/1` | Value |
//! |---|---|
//! | header | `schema`, `id` (= the file stem), `version`, `synthetic = true`, `profile` (the profile id it holds for), `note` |
//! | `week`, `decided_at` | the cycle id · the decision time |
//! | `after?` · `active?` | a case run first in the same state (weekly rerun) · opportunity id → next stage |
//! | `[[records]]` | test sources (`domain::source::testkit`): `src` (`SEC` `TED` `WIRE` `DAILY` `SOCIAL` `FORUM` `REPO`), `native`, `published`, `observed?` (default + 5 min), `copy_of?` (a syndicated copy of that native), `report_of?` (an independent report of its event), `valid_until?`, `stored?` (default true; false = only the origin of copies) |
//! | `[[proposals]]` · `[[challenges]]` | what the Architect / Critic stage wrote: `ProposalDraft` / `ChallengeDraft` exactly as a model writes them, `"@<native>"` = that record's id |
//! | `[expected]` | `hold`, `allocation`, `ranked` / `held` / `rejected` rows (`id`, `action` = a kind or the whole action, `gates?`), `confirmations?` (native → independent confirmations), `next_information?` |
//!
//! Fakes: [`MemCycleStore`] (a `CycleStore` in memory, freeze = read-only),
//! [`MemSourceStore`] (every record: the as-of view makes the cut — the
//! SQLite store's seed read equals the whole world, `application::sources`
//! tests), [`ScriptedRunner`] (submits recorded drafts through the tools'
//! path, `submit_*`, like a cached stage run).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use super::cycle::{
    run_cycle, CycleEnv, CycleOutcome, CycleParams, ProfileIn, Target, CANDIDATES, DECIDED,
    EPISODES, FAILED, FORECAST, FORECAST_LINE, INPUTS, MEMO, PORTFOLIO, STAGES,
};
use super::freeze::{decision_sha256, verify, verify_state, FileCheck, LogState, OPS};
use super::submit::{submit_challenge, submit_proposal, GenerationPin, PACKET};
use crate::application::runtime::{keep_lease, HeldLeases, LeaseTiming, Supervisor};
use crate::config::soe::{load_profile, Loaded};
use crate::config::sources::SourcesConfig;
use crate::domain::canonical::canonical_json;
use crate::domain::lineage::value::Time;
use crate::domain::metrics::{MetricsKind, MetricsRecord};
use crate::domain::observation::now_ms;
use crate::domain::runtime::{Heartbeat, RunnerLease};
use crate::domain::soe::challenge::Challenge;
use crate::domain::soe::episode::OpportunityEpisode;
use crate::domain::soe::forecast::{verify_chain, Forecast, LogLine};
use crate::domain::soe::ops::{Cost, CycleOps, Stage, TokenPrices};
use crate::domain::soe::portfolio::{Allocation, PortfolioAction, WeeklyPortfolio};
use crate::domain::soe::profile::OperatorProfile;
use crate::domain::soe::proposal::{MechanismProposal, Provenance};
use crate::domain::soe::record::from_json;
use crate::domain::soe::value::{codes, Currency, Est, Minor};
use crate::domain::source::testkit::{copy_of, rec, Src};
use crate::domain::source::{Coverage, Purge, SourceRecord};
use crate::ports::clock::SimClock;
use crate::ports::runtime::{LeaseLost, Ownership, RuntimeStore, Unleased};
use crate::ports::soe::{
    valid_file_name, CycleStore, RunDir, RunStatus, StageReply, StageRequest, StageRunner,
    StateLog, CHALLENGES, MANIFEST, PROPOSALS,
};
use crate::ports::source_store::{
    Batch, CommitReport, Cursor, PurgeRequest, RecordQuery, Snapshot, SourceStore, SourceSwitch,
};

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Mem {
    pub dirs: BTreeMap<RunDir, BTreeMap<String, Vec<u8>>>,
    pub frozen: BTreeSet<RunDir>,
    pub logs: BTreeMap<StateLog, Vec<String>>,
}

/// A `CycleStore` in memory (the port's module table, freeze = read-only).
#[derive(Debug, Default)]
pub(crate) struct MemCycleStore {
    pub mem: Mutex<Mem>,
}

impl MemCycleStore {
    pub(crate) fn snapshot(&self) -> Mem {
        self.mem.lock().unwrap().clone()
    }

    pub(crate) fn dir(&self, dir: &RunDir) -> BTreeMap<String, Vec<u8>> {
        self.snapshot().dirs.get(dir).cloned().unwrap_or_default()
    }

    /// Change a file behind the store's back (a tamper test).
    pub(crate) fn tamper(&self, dir: &RunDir, name: &str, bytes: &[u8]) {
        let mut m = self.mem.lock().unwrap();
        m.dirs
            .get_mut(dir)
            .unwrap()
            .insert(name.to_string(), bytes.to_vec());
    }

    fn append(&self, dir: &RunDir, name: &str, line: String) -> Result<()> {
        let mut m = self.mem.lock().unwrap();
        if m.frozen.contains(dir) {
            bail!("{dir} is frozen");
        }
        let Some(files) = m.dirs.get_mut(dir) else {
            bail!("{dir} is not claimed");
        };
        let f = files.entry(name.to_string()).or_default();
        f.extend_from_slice(line.as_bytes());
        f.push(b'\n');
        Ok(())
    }

    fn records<T: crate::domain::soe::record::SoeRecord + serde::de::DeserializeOwned>(
        &self,
        dir: &RunDir,
        name: &str,
    ) -> Result<Vec<T>> {
        let bytes = self.dir(dir).get(name).cloned().unwrap_or_default();
        let text = String::from_utf8(bytes)?;
        text.lines()
            .enumerate()
            .map(|(i, l)| {
                from_json::<T>(l).map_err(|e| anyhow::anyhow!("{dir}/{name} line {}: {e:?}", i + 1))
            })
            .collect()
    }
}

impl CycleStore for MemCycleStore {
    fn root_display(&self) -> String {
        "mem:soe".into()
    }

    fn status(&self, dir: &RunDir) -> Result<RunStatus> {
        let m = self.mem.lock().unwrap();
        Ok(if m.frozen.contains(dir) {
            RunStatus::Frozen
        } else if m.dirs.contains_key(dir) {
            RunStatus::Open
        } else {
            RunStatus::Absent
        })
    }

    fn claim(&self, dir: &RunDir) -> Result<()> {
        let mut m = self.mem.lock().unwrap();
        if m.dirs.contains_key(dir) {
            bail!("{dir} exists");
        }
        m.dirs.insert(dir.clone(), BTreeMap::new());
        Ok(())
    }

    fn cycles(&self) -> Result<Vec<String>> {
        let m = self.mem.lock().unwrap();
        Ok(m.dirs
            .keys()
            .filter(|d| matches!(d, RunDir::Cycle(_)))
            .map(|d| d.id().to_string())
            .collect())
    }

    fn write(&self, dir: &RunDir, name: &str, bytes: &[u8]) -> Result<()> {
        if !valid_file_name(name) {
            bail!("`{name}` is not a run file name");
        }
        let mut m = self.mem.lock().unwrap();
        if m.frozen.contains(dir) {
            bail!("{dir} is frozen");
        }
        let Some(files) = m.dirs.get_mut(dir) else {
            bail!("{dir} is not claimed");
        };
        if files.contains_key(name) {
            bail!("{dir}/{name} exists");
        }
        files.insert(name.to_string(), bytes.to_vec());
        Ok(())
    }

    fn read(&self, dir: &RunDir, name: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.dir(dir).get(name).cloned())
    }

    fn files(&self, dir: &RunDir) -> Result<Vec<String>> {
        Ok(self
            .dir(dir)
            .into_keys()
            .filter(|n| n != MANIFEST)
            .collect())
    }

    fn append_proposal(&self, dir: &RunDir, p: &MechanismProposal) -> Result<()> {
        self.append(dir, PROPOSALS, canonical_json(&serde_json::to_value(p)?))
    }

    fn proposals(&self, dir: &RunDir) -> Result<Vec<MechanismProposal>> {
        self.records(dir, PROPOSALS)
    }

    fn append_challenge(&self, dir: &RunDir, c: &Challenge) -> Result<()> {
        self.append(dir, CHALLENGES, canonical_json(&serde_json::to_value(c)?))
    }

    fn challenges(&self, dir: &RunDir) -> Result<Vec<Challenge>> {
        self.records(dir, CHALLENGES)
    }

    fn freeze(&self, dir: &RunDir, manifest: &[u8]) -> Result<()> {
        let mut m = self.mem.lock().unwrap();
        let Some(files) = m.dirs.get_mut(dir) else {
            bail!("{dir} is not claimed");
        };
        if files.contains_key(MANIFEST) {
            bail!("{dir} is frozen");
        }
        files.insert(MANIFEST.into(), manifest.to_vec());
        m.frozen.insert(dir.clone());
        Ok(())
    }

    fn append_line(&self, log: StateLog, line: &str) -> Result<u64> {
        if line.contains('\n') {
            bail!("a log line holds no newline");
        }
        let mut m = self.mem.lock().unwrap();
        let l = m.logs.entry(log).or_default();
        l.push(line.to_string());
        Ok(l.len() as u64)
    }

    fn lines(&self, log: StateLog) -> Result<Vec<String>> {
        Ok(self
            .mem
            .lock()
            .unwrap()
            .logs
            .get(&log)
            .cloned()
            .unwrap_or_default())
    }
}

/// Every record it holds, whatever the query (module doc).
#[derive(Debug, Default)]
pub(crate) struct MemSourceStore {
    pub records: Mutex<Vec<SourceRecord>>,
}

impl MemSourceStore {
    pub(crate) fn with(records: Vec<SourceRecord>) -> Self {
        Self {
            records: Mutex::new(records),
        }
    }

    pub(crate) fn add(&self, more: &[SourceRecord]) {
        self.records.lock().unwrap().extend_from_slice(more);
    }
}

#[async_trait]
impl SourceStore for MemSourceStore {
    async fn commit(&self, _: Batch) -> Result<CommitReport> {
        bail!("not used by the cycle")
    }
    async fn records(&self, _: &RecordQuery) -> Result<Vec<SourceRecord>> {
        let mut r = self.records.lock().unwrap().clone();
        r.sort_by(|a, b| {
            (a.observed_ms, a.parsed_ms, &a.record_id).cmp(&(
                b.observed_ms,
                b.parsed_ms,
                &b.record_id,
            ))
        });
        Ok(r)
    }
    async fn coverage(&self, _: Option<&str>) -> Result<Vec<Coverage>> {
        Ok(Vec::new())
    }
    async fn cursor(&self, _: &str, _: &str) -> Result<Option<Cursor>> {
        Ok(None)
    }
    async fn cursors(&self, _: Option<&str>) -> Result<Vec<Cursor>> {
        Ok(Vec::new())
    }
    async fn snapshot(&self, _: &str) -> Result<Option<Snapshot>> {
        Ok(None)
    }
    async fn purge(&self, _: &PurgeRequest) -> Result<Purge> {
        bail!("not used by the cycle")
    }
    async fn purges(&self, _: Option<&str>) -> Result<Vec<Purge>> {
        Ok(Vec::new())
    }
    async fn set_switch(&self, _: &SourceSwitch) -> Result<()> {
        bail!("not used by the cycle")
    }
    async fn switches(&self, _: Option<&str>) -> Result<Vec<SourceSwitch>> {
        Ok(Vec::new())
    }
}

/// How a scripted stage ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ending {
    Ok,
    /// `ok = false` after the drafts (the child failed late).
    Failed,
    /// The runner itself fails before anything is written.
    Err,
}

/// A stage runner that submits recorded drafts through the tools' path
/// (`submit_*`) — a cached stage run.
pub(crate) struct ScriptedRunner {
    pub store: Arc<MemCycleStore>,
    pub proposals: Vec<String>,
    pub challenges: Vec<String>,
    pub architect: Ending,
    pub critic: Ending,
    /// What each run reports (measurement only: `ops.json`).
    pub latency_ms: u64,
    pub requests: Mutex<Vec<StageRequest>>,
    /// Refusals the tools answered, by stage.
    pub refused: Mutex<Vec<(Stage, Vec<String>)>>,
}

impl ScriptedRunner {
    pub(crate) fn new(
        store: Arc<MemCycleStore>,
        proposals: Vec<String>,
        challenges: Vec<String>,
    ) -> Self {
        Self {
            store,
            proposals,
            challenges,
            architect: Ending::Ok,
            critic: Ending::Ok,
            latency_ms: 30_000,
            requests: Mutex::new(Vec::new()),
            refused: Mutex::new(Vec::new()),
        }
    }
}

pub(crate) fn stamp(stage: Stage, agent: &str, n: usize) -> Provenance {
    Provenance {
        agent: agent.into(),
        model: "synthetic-model".into(),
        engine: "openrouter".into(),
        skill_sha256: format!("{:064x}", stage as u8 + 1),
        generation: "SOE-G0".into(),
        call_id: format!("soe:{agent}:{n}"),
        proposed_at: "2026-10-05T12:30:00Z".parse().unwrap(),
    }
}

#[async_trait]
impl StageRunner for ScriptedRunner {
    async fn run(&self, req: &StageRequest) -> Result<StageReply> {
        self.requests.lock().unwrap().push(req.clone());
        let (drafts, ending) = match req.stage {
            Stage::Architect => (&self.proposals, self.architect),
            _ => (&self.challenges, self.critic),
        };
        if ending == Ending::Err {
            bail!("no child: the synthetic runner was told to fail");
        }
        let mut written = 0;
        for (i, d) in drafts.iter().enumerate() {
            let p = stamp(req.stage, &req.agent, i + 1);
            let refused = match req.stage {
                Stage::Architect => submit_proposal(&*self.store, &req.dir, d, p)?.err(),
                _ => submit_challenge(&*self.store, &req.dir, d, p)?.err(),
            };
            match refused {
                None => written += 1,
                Some(e) => self
                    .refused
                    .lock()
                    .unwrap()
                    .push((req.stage, e.iter().map(ToString::to_string).collect())),
            }
        }
        let ok = ending == Ending::Ok;
        Ok(StageReply {
            ok,
            summary: format!("{written} records written"),
            error: (!ok).then(|| "the synthetic child exited 1".to_string()),
            latency_ms: self.latency_ms,
            metrics: vec![MetricsRecord {
                ts_unix: 0,
                session_id: req.cycle_id.clone(),
                kind: MetricsKind::Subagent,
                agent: req.agent.clone(),
                model: "synthetic-model".into(),
                prompt_tokens: 12_000,
                completion_tokens: 900,
                total_tokens: 12_900,
                prompt_chars: 0,
                prompt_bytes: 0,
                response_chars: 0,
                latency_ms: self.latency_ms,
                layers: Vec::new(),
                step_id: None,
            }],
        })
    }
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/soe")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordSpec {
    pub src: String,
    pub native: String,
    pub published: Time,
    #[serde(default)]
    pub observed: Option<Time>,
    #[serde(default)]
    pub copy_of: Option<String>,
    #[serde(default)]
    pub report_of: Option<String>,
    #[serde(default)]
    pub valid_until: Option<Time>,
    #[serde(default = "yes")]
    pub stored: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Row {
    pub id: String,
    /// A kind (`"HOLD"`) or the whole action.
    pub action: toml::Value,
    #[serde(default)]
    pub gates: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Expected {
    pub hold: bool,
    pub allocation: Allocation,
    pub ranked: Vec<Row>,
    pub held: Vec<Row>,
    pub rejected: Vec<Row>,
    #[serde(default)]
    pub confirmations: BTreeMap<String, usize>,
    #[serde(default)]
    pub next_information: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CycleCase {
    pub schema: String,
    pub id: String,
    pub version: u32,
    pub synthetic: bool,
    pub profile: String,
    pub note: String,
    pub week: String,
    pub decided_at: Time,
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub active: BTreeMap<String, usize>,
    #[serde(default)]
    pub records: Vec<RecordSpec>,
    pub expected: Expected,
    #[serde(default)]
    pub proposals: Vec<toml::Value>,
    #[serde(default)]
    pub challenges: Vec<toml::Value>,
}

pub(crate) fn load_case(id: &str) -> CycleCase {
    let path = root().join("cycles").join(format!("{id}.toml"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let c: CycleCase = toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(c.schema, "soe.cycle_case/1", "{id}");
    assert_eq!(c.id, id, "the id is the file stem");
    assert!(c.version >= 1 && c.synthetic, "{id}: synthetic cases only");
    assert_eq!(c.profile, "synthetic-operator", "{id}");
    assert!(!c.note.trim().is_empty(), "{id}: a note");
    c
}

pub(crate) fn case_ids() -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(root().join("cycles"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    ids.sort();
    ids
}

fn src(name: &str) -> Src {
    match name {
        "SEC" => Src::SEC,
        "TED" => Src::TED,
        "WIRE" => Src::WIRE,
        "DAILY" => Src::DAILY,
        "SOCIAL" => Src::SOCIAL,
        "FORUM" => Src::FORUM,
        "REPO" => Src::REPO,
        other => panic!("unknown test source `{other}`"),
    }
}

pub(crate) const MIN: i64 = 60_000;

fn ms(t: &Time) -> i64 {
    t.earliest().expect("a known time")
}

/// The records of `specs`, by native id (`stored` ones in order).
pub(crate) fn build_records(
    specs: &[RecordSpec],
) -> (BTreeMap<String, SourceRecord>, Vec<SourceRecord>) {
    let mut by: BTreeMap<String, SourceRecord> = BTreeMap::new();
    let mut stored = Vec::new();
    for s in specs {
        let s_ = src(&s.src);
        let published = ms(&s.published);
        let observed = s.observed.as_ref().map(ms).unwrap_or(published + 5 * MIN);
        let mut r = match (&s.copy_of, &s.report_of) {
            (Some(of), None) => copy_of(&by[of], s_, &s.native, published, observed),
            (None, Some(of)) => {
                let mut r = rec(s_, &s.native, published, observed, observed + MIN);
                r.event_key = by[of].event_key.clone();
                r.with_identity()
            }
            (None, None) => rec(s_, &s.native, published, observed, observed + MIN),
            (Some(_), Some(_)) => panic!("{}: copy_of or report_of, not both", s.native),
        };
        if let Some(u) = &s.valid_until {
            r.valid_until_ms = Some(ms(u));
            r = r.with_identity();
        }
        assert!(
            by.insert(s.native.clone(), r.clone()).is_none(),
            "{}: twice",
            s.native
        );
        if s.stored {
            stored.push(r);
        }
    }
    (by, stored)
}

/// The records of `case` and of every case run before it (`after`), oldest first.
pub(crate) fn chain_specs(case: &CycleCase) -> Vec<RecordSpec> {
    let mut specs = match &case.after {
        Some(prev) => chain_specs(&load_case(prev)),
        None => Vec::new(),
    };
    specs.extend(case.records.iter().cloned());
    specs
}

/// `"@<native>"` → that record's id, everywhere in `v`.
fn resolve(v: &mut Value, by: &BTreeMap<String, SourceRecord>) {
    match v {
        Value::String(s) => {
            if let Some(native) = s.strip_prefix('@') {
                let r = by
                    .get(native)
                    .unwrap_or_else(|| panic!("`@{native}`: no such record"));
                *s = r.record_id.clone();
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| resolve(x, by)),
        Value::Object(m) => m.values_mut().for_each(|x| resolve(x, by)),
        _ => {}
    }
}

/// The drafts as the model's JSON text, ids resolved.
pub(crate) fn drafts(items: &[toml::Value], by: &BTreeMap<String, SourceRecord>) -> Vec<String> {
    items
        .iter()
        .map(|t| {
            let mut v = serde_json::to_value(t).unwrap();
            resolve(&mut v, by);
            v.to_string()
        })
        .collect()
}

/// The `[sources]` registry of the test sources.
pub(crate) fn registry() -> SourcesConfig {
    let mut text = "state = \"soe\"\n".to_string();
    for s in [
        Src::SEC,
        Src::TED,
        Src::WIRE,
        Src::DAILY,
        Src::SOCIAL,
        Src::FORUM,
        Src::REPO,
    ] {
        text.push_str(&format!(
            "[registry.{}]\nkind = \"ted_search\"\nclass = \"law_regulator\"\ntrust = \"primary\"\n\
             revision = \"{}\"\nenabled = false\nhosts = [\"example.org\"]\nauth = \"none\"\n\
             rate_limit = \"x\"\nstore_raw = false\njurisdiction = \"EU\"\nlanguage = \"en\"\n",
            s.id,
            s.revision.as_str()
        ));
        if let Some(age) = s.policy().listing_max_age_ms {
            text.push_str(&format!("listing_max_age_days = {}\n", age / 86_400_000));
        }
    }
    toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"))
}

pub(crate) fn synthetic_profile() -> Loaded<OperatorProfile> {
    load_profile(&root().join("profile.synthetic.toml"), true).unwrap()
}

pub(crate) fn generation() -> GenerationPin {
    GenerationPin {
        id: "SOE-G0".into(),
        sha256: "0000000000000000000000000000000000000000000000000000000000000007".into(),
    }
}

/// One SOE state: the stores, the clock, the profile.
pub(crate) struct Bench {
    pub store: Arc<MemCycleStore>,
    pub sources: MemSourceStore,
    pub registry: SourcesConfig,
    pub clock: SimClock,
    pub profile: Loaded<OperatorProfile>,
    pub text: String,
}

pub(crate) const CLOCK_MS: i64 = 1_791_000_000_000;

impl Bench {
    pub(crate) fn new() -> Bench {
        let profile = synthetic_profile();
        let text = std::fs::read_to_string(&profile.path).unwrap();
        Bench {
            store: Arc::new(MemCycleStore::default()),
            sources: MemSourceStore::default(),
            registry: registry(),
            clock: SimClock::at(CLOCK_MS),
            profile,
            text,
        }
    }

    /// This state under a synthetic profile with another weekly budget.
    pub(crate) fn with_budget(mut self, hours: u32, tranche: &str) -> Bench {
        let text = self
            .text
            .replace(
                "weekly_owner_hours = 10",
                &format!("weekly_owner_hours = {hours}"),
            )
            .replace(
                "max_validation_tranche = \"1500.00\"",
                &format!("max_validation_tranche = \"{tranche}\""),
            );
        self.profile.record = crate::domain::soe::record::from_toml(&text).unwrap();
        assert_eq!(
            (
                self.profile.record.weekly_owner_hours,
                self.profile.record.max_validation_tranche,
            ),
            (hours, tranche.parse().unwrap()),
            "budget lines not found"
        );
        self.profile.sha256 = crate::domain::lineage::pins::toml_digest(&text).unwrap();
        self.text = text;
        self
    }

    pub(crate) fn params(&self, week: &str, decided_at: i64, target: Target) -> CycleParams {
        CycleParams {
            target,
            week: week.parse().unwrap(),
            decided_at_ms: decided_at,
            generation: generation(),
            architect: "soe-architect".into(),
            critic: "soe-critic".into(),
            max_proposals: 12,
            forecast_max_weeks: 12,
            active: BTreeMap::new(),
            token_prices: None,
        }
    }

    pub(crate) async fn cycle(
        &self,
        runner: Option<&dyn StageRunner>,
        p: &CycleParams,
    ) -> Result<CycleOutcome> {
        self.cycle_under(runner, p, &Unleased).await
    }

    /// [`Bench::cycle`] writing under `owner`'s leases.
    pub(crate) async fn cycle_under(
        &self,
        runner: Option<&dyn StageRunner>,
        p: &CycleParams,
        owner: &dyn Ownership,
    ) -> Result<CycleOutcome> {
        let env = CycleEnv {
            sources: Some(&self.sources),
            registry: &self.registry,
            store: &*self.store,
            runner,
            clock: &self.clock,
            profile: ProfileIn {
                record: &self.profile.record,
                sha256: &self.profile.sha256,
                text: &self.text,
            },
            owner,
        };
        run_cycle(&env, p).await
    }

    /// Run `case` (its `after` chain first) in this state; no tool refusal.
    pub(crate) async fn run_case(
        &self,
        case: &CycleCase,
        target: Target,
    ) -> (CycleOutcome, ScriptedRunner) {
        let (out, runner) = self.try_case(case, target, |_| {}).await;
        let out = out.unwrap_or_else(|e| panic!("{}: {e:#}", case.id));
        let refused = runner.refused.lock().unwrap().clone();
        assert!(
            refused.is_empty(),
            "{}: the tools refused {refused:?}",
            case.id
        );
        (out, runner)
    }

    /// Store `case`'s records, script its stages (`set` adjusts the runner)
    /// and run it; its `after` chain runs first, plainly.
    pub(crate) async fn try_case(
        &self,
        case: &CycleCase,
        target: Target,
        set: impl FnOnce(&mut ScriptedRunner),
    ) -> (Result<CycleOutcome>, ScriptedRunner) {
        if let Some(prev) = &case.after {
            let before = load_case(prev);
            Box::pin(self.run_case(&before, Target::Cycle)).await;
        }
        let (mut runner, p) = self.script(case, target);
        set(&mut runner);
        let out = self.cycle(Some(&runner), &p).await;
        (out, runner)
    }

    /// Store `case`'s records (not its `after` chain) and script its
    /// stages: the runner and the cycle's params.
    pub(crate) fn script(&self, case: &CycleCase, target: Target) -> (ScriptedRunner, CycleParams) {
        let (by, _) = build_records(&chain_specs(case));
        let own: BTreeSet<&str> = case.records.iter().map(|r| r.native.as_str()).collect();
        let stored: Vec<SourceRecord> = case
            .records
            .iter()
            .filter(|r| r.stored)
            .map(|r| by[&r.native].clone())
            .collect();
        debug_assert!(stored.iter().all(|r| own.contains(r.native_id.as_str())));
        self.sources.add(&stored);
        let runner = ScriptedRunner::new(
            self.store.clone(),
            drafts(&case.proposals, &by),
            drafts(&case.challenges, &by),
        );
        let mut p = self.params(&case.week, ms(&case.decided_at), target);
        p.active = case.active.clone();
        (runner, p)
    }
}

fn row_matches(
    want: &Row,
    id: &str,
    action: &PortfolioAction,
    gates: Option<&[String]>,
) -> Result<(), String> {
    if want.id != id {
        return Err(format!("id {id}, want {}", want.id));
    }
    match &want.action {
        toml::Value::String(kind) if kind == action.kind() => {}
        toml::Value::String(kind) => {
            return Err(format!("{id}: action {}, want {kind}", action.kind()))
        }
        t => {
            let w: PortfolioAction = serde_json::from_value(serde_json::to_value(t).unwrap())
                .unwrap_or_else(|e| panic!("{id}: expected action: {e}"));
            if &w != action {
                return Err(format!("{id}: action {action:?}, want {w:?}"));
            }
        }
    }
    if let (Some(w), Some(g)) = (&want.gates, gates) {
        if w.as_slice() != g {
            return Err(format!("{id}: gates {g:?}, want {w:?}"));
        }
    }
    Ok(())
}

/// A portfolio list as `(id, action, gates)` rows.
type Rows<'a> = Vec<(&'a str, &'a PortfolioAction, Option<&'a [String]>)>;

/// Every difference between `case.expected` and the run.
pub(crate) fn diffs(case: &CycleCase, out: &CycleOutcome, bench: &Bench) -> Vec<String> {
    let e = &case.expected;
    let p = &out.portfolio;
    let mut d = Vec::new();
    if p.is_hold() != e.hold {
        d.push(format!("hold {}, want {}", p.is_hold(), e.hold));
    }
    if p.allocation != e.allocation {
        d.push(format!(
            "allocation {:?}, want {:?}",
            p.allocation, e.allocation
        ));
    }
    let lists: [(&str, &[Row], Rows); 3] = [
        (
            "ranked",
            &e.ranked,
            p.ranked
                .iter()
                .map(|r| (r.id.as_str(), &r.action, None))
                .collect(),
        ),
        (
            "held",
            &e.held,
            p.held
                .iter()
                .map(|r| (r.id.as_str(), &r.action, Some(r.gates.as_slice())))
                .collect(),
        ),
        (
            "rejected",
            &e.rejected,
            p.rejected
                .iter()
                .map(|r| (r.id.as_str(), &r.action, Some(r.gates.as_slice())))
                .collect(),
        ),
    ];
    for (name, want, got) in lists {
        if want.len() != got.len() {
            d.push(format!(
                "{name}: {:?}, want {:?}",
                got.iter()
                    .map(|(id, a, _)| format!("{id}:{}", a.kind()))
                    .collect::<Vec<_>>(),
                want.iter().map(|r| r.id.as_str()).collect::<Vec<_>>()
            ));
            continue;
        }
        for (w, (id, a, g)) in want.iter().zip(got) {
            if let Err(why) = row_matches(w, id, a, g) {
                d.push(format!("{name}: {why}"));
            }
        }
    }
    if let Some(n) = &e.next_information {
        if &p.next_information != n {
            d.push(format!(
                "next_information {:?}, want {n:?}",
                p.next_information
            ));
        }
    }
    if !e.confirmations.is_empty() {
        let (by, _) = build_records(&chain_specs(case));
        let packet: crate::domain::source::EvidencePacket =
            serde_json::from_slice(&bench.store.dir(&out.dir)[PACKET]).unwrap();
        let idx = crate::domain::soe::observe::EvidenceIndex::of(&packet);
        for (native, n) in &e.confirmations {
            let got = idx.get(&by[native].record_id).map(|r| r.confirmations);
            if got != Some(*n) {
                d.push(format!("confirmations of {native}: {got:?}, want {n}"));
            }
        }
    }
    d
}

fn file<'a>(files: &'a BTreeMap<String, Vec<u8>>, name: &str) -> &'a str {
    std::str::from_utf8(files.get(name).unwrap_or_else(|| panic!("{name} missing"))).unwrap()
}

fn json_file(files: &BTreeMap<String, Vec<u8>>, name: &str) -> Value {
    serde_json::from_str(file(files, name)).unwrap()
}

fn t(s: &str) -> i64 {
    s.parse::<Time>().unwrap().earliest().unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Every synthetic case runs end to end (stages through the tools' path,
/// decided, reported, frozen) and answers as `[expected]` says.
#[tokio::test]
async fn acceptance_cases() {
    let ids = case_ids();
    assert!(ids.len() >= 10, "cases: {ids:?}");
    let mut failures = Vec::new();
    for id in &ids {
        let case = load_case(id);
        let bench = Bench::new();
        let (out, _) = bench.run_case(&case, Target::Cycle).await;
        let d = diffs(&case, &out, &bench);
        if !d.is_empty() {
            failures.push(format!("{id}:\n  {}", d.join("\n  ")));
        }
        // Every run is frozen and verifies.
        let v = verify(&*bench.store, &out.dir).unwrap();
        assert!(v.ok(), "{id}: {v:?}");
        // No launch action exists; nothing exceeds the week.
        let prof = &bench.profile.record;
        assert!(
            out.portfolio.allocation.owner_hours <= prof.weekly_owner_hours,
            "{id}"
        );
        assert!(
            out.portfolio.allocation.cash <= prof.max_validation_tranche,
            "{id}"
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Fixture authoring aid: `SOE_CASE=<id> cargo test --bin tengu
/// application::soe::tests::print_case -- --ignored --nocapture` prints the
/// case's portfolio, its diffs and the memo.
#[tokio::test]
#[ignore]
async fn print_case() {
    let id = std::env::var("SOE_CASE").expect("SOE_CASE=<case id>");
    let case = load_case(&id);
    let bench = Bench::new();
    let (out, runner) = bench.run_case(&case, Target::Cycle).await;
    let files = bench.store.dir(&out.dir);
    println!("{}", serde_json::to_string_pretty(&out.portfolio).unwrap());
    println!("{}", file(&files, DECIDED));
    println!("{}", file(&files, MEMO));
    println!("refused: {:?}", runner.refused.lock().unwrap());
    println!("diffs: {:#?}", diffs(&case, &out, &bench));
}

fn lines_of(files: &BTreeMap<String, Vec<u8>>, name: &str) -> Vec<Value> {
    file(files, name)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Roadmap O3 exit "`HOLD` passes end-to-end": the strong-news week goes
/// through every stage, holds, is frozen with its forecast, and learns.
#[tokio::test]
async fn hold_end_to_end() {
    let case = load_case("strong_news_weak_demand");
    let bench = Bench::new();
    let (out, runner) = bench.run_case(&case, Target::Cycle).await;
    let dir = RunDir::Cycle("2026-W41".into());
    assert_eq!(out.dir, dir);
    assert!(out.portfolio.is_hold() && out.portfolio.ranked.is_empty());
    assert_eq!(bench.store.status(&dir).unwrap(), RunStatus::Frozen);
    let files = bench.store.dir(&dir);
    let names: Vec<&str> = files.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "MANIFEST.json",
            CANDIDATES,
            "carried.json",
            DECIDED,
            EPISODES,
            FORECAST_LINE,
            FORECAST,
            "head.json",
            INPUTS,
            MEMO,
            OPS,
            "packet.json",
            "phase-challenge.json",
            "phase-closed.json",
            "phase-propose.json",
            PORTFOLIO,
            "profile.toml",
            PROPOSALS,
            STAGES,
        ]
    );
    // Frozen: every file verifies, nothing more can be written.
    let v = verify(&*bench.store, &dir).unwrap();
    assert!(v.ok(), "{v:?}");
    assert_eq!(
        v.manifest_sha256.as_deref(),
        Some(out.manifest_sha256.as_str())
    );
    assert!(bench.store.write(&dir, "late.json", b"{}").is_err());
    let p =
        from_json::<MechanismProposal>(file(&files, PROPOSALS).lines().next().unwrap()).unwrap();
    assert_eq!(p.id, "2026-W41.p01");
    assert!(bench.store.append_proposal(&dir, &p).is_err());

    // The portfolio file is the outcome; the memo states the HOLD week.
    let pf: WeeklyPortfolio = serde_json::from_str(file(&files, PORTFOLIO)).unwrap();
    assert_eq!(pf, out.portfolio);
    let memo = file(&files, MEMO);
    for section in [
        "## Verdict",
        "## Facts",
        "## Inference",
        "## Computed",
        "## Unsupported claims",
        "## Unknowns",
        "## Challenges",
        "## Next information worth buying",
        "## Cycle",
    ] {
        assert!(memo.contains(section), "{section}");
    }
    assert!(memo.contains("`HOLD` week"), "{memo}");
    assert!(
        memo.contains("| `economics.initial.working_capital` |")
            || memo.contains("economics.initial.working_capital")
    );

    // The forecast froze at the decision with the portfolio; one chained line.
    let f: Forecast = from_json(file(&files, FORECAST).trim_end()).unwrap();
    assert_eq!(f.frozen_at, case.decided_at);
    assert_eq!(
        f.portfolio_sha256,
        crate::domain::soe::forecast::sha256_of(&out.portfolio).unwrap()
    );
    assert_eq!(f.items.len(), 1);
    let log: Vec<LogLine> = bench
        .store
        .lines(StateLog::ForecastLog)
        .unwrap()
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].prev_sha256, crate::domain::soe::forecast::GENESIS);
    assert_eq!(
        serde_json::to_value(&log[0]).unwrap(),
        json_file(&files, FORECAST_LINE)
    );
    assert_eq!(verify_chain(&log), Ok(()));

    // Learn: one episode and one event, in the dir and in the state logs.
    let episodes = bench.store.lines(StateLog::Episodes).unwrap();
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0], file(&files, EPISODES).trim_end());
    let e: OpportunityEpisode = from_json(&episodes[0]).unwrap();
    assert_eq!(
        (e.id.as_str(), e.opportunity.as_str(), e.verdict),
        (
            "2026-W41.e01",
            "news-automation",
            crate::domain::soe::record::Verdict::Hold
        )
    );
    assert!(e.actions.is_empty() && e.spend == Minor::ZERO && e.owner_hours == 0);
    assert!(e
        .quality
        .decision_note
        .starts_with("CHEAP_TEST: SINGLE_DEMAND_SIGNAL"));
    assert_eq!(
        e.actual.monthly_cash,
        Est::unknown("shadow cycle: no action ran")
    );
    let events = bench.store.lines(StateLog::Candidates).unwrap();
    assert_eq!(events.len(), 1);
    let ev: Value = serde_json::from_str(&events[0]).unwrap();
    assert_eq!(
        (
            ev["event"].as_str(),
            ev["list"].as_str(),
            ev["episode"].as_str()
        ),
        (Some("DECIDED"), Some("HELD"), Some("2026-W41.e01"))
    );

    // Identity: inputs, policy and generation recorded and hashed.
    let inputs = json_file(&files, INPUTS);
    let mut rest = inputs.clone();
    rest.as_object_mut().unwrap().remove("inputs_sha256");
    assert_eq!(
        inputs["inputs_sha256"].as_str(),
        Some(crate::domain::canonical::canonical_sha256(&rest).as_str())
    );
    assert_eq!(
        inputs["inputs_sha256"].as_str(),
        Some(out.inputs_sha256.as_str())
    );
    assert_eq!(
        inputs["policy_sha256"].as_str(),
        Some(super::cycle::policy_sha256(&bench.profile.record.rank_order).as_str())
    );
    assert_eq!(inputs["generation"]["id"], "SOE-G0");
    assert_eq!(
        inputs["profile"]["sha256"].as_str(),
        Some(bench.profile.sha256.as_str())
    );
    assert_eq!(inputs["mode"], "captured");
    assert_eq!(inputs["proposals"][0]["id"], "2026-W41.p01");

    // Stages and ops: both model stages ran; cost unknown without prices.
    let ops: CycleOps = serde_json::from_str(file(&files, OPS)).unwrap();
    let stages: Vec<Stage> = ops.stages.iter().map(|s| s.stage).collect();
    assert_eq!(
        stages,
        [
            Stage::Observe,
            Stage::Architect,
            Stage::Challenge,
            Stage::Allocate,
            Stage::Report
        ]
    );
    assert_eq!(
        (ops.prompt_tokens, ops.completion_tokens, ops.failed_stages),
        (24_000, 1_800, 0)
    );
    assert_eq!(json_file(&files, OPS)["cost"]["kind"], "UNKNOWN");
    assert_eq!(runner.requests.lock().unwrap().len(), 2);
}

/// Roadmap § 13 reproducibility: the same packet, profile, generation and
/// recorded stages give the same bytes — every case, file by file; a run
/// that only measured another latency differs in `ops.json` alone.
#[tokio::test]
async fn cycle_rerun_is_byte_identical() {
    for id in case_ids() {
        let case = load_case(&id);
        let (a, b) = (Bench::new(), Bench::new());
        let (oa, _) = a.run_case(&case, Target::Cycle).await;
        let (ob, _) = b.run_case(&case, Target::Cycle).await;
        assert_eq!(
            a.store.snapshot(),
            b.store.snapshot(),
            "{id}: state differs"
        );
        assert_eq!(
            (&oa.inputs_sha256, &oa.decision_sha256, &oa.manifest_sha256),
            (&ob.inputs_sha256, &ob.decision_sha256, &ob.manifest_sha256),
            "{id}"
        );
        let c = Bench::new();
        let (oc, _) = c
            .try_case(&case, Target::Cycle, |r| r.latency_ms = 41_000)
            .await;
        let oc = oc.unwrap();
        assert_eq!(oc.decision_sha256, oa.decision_sha256, "{id}");
        assert_ne!(oc.manifest_sha256, oa.manifest_sha256, "{id}");
        let (fa, fc) = (a.store.dir(&oa.dir), c.store.dir(&oc.dir));
        let differ: Vec<&String> = fa.keys().filter(|k| fa.get(*k) != fc.get(*k)).collect();
        assert_eq!(
            differ,
            [&"MANIFEST.json".to_string(), &OPS.to_string()],
            "{id}"
        );
        assert_eq!(
            decision_sha256(&*c.store, &oc.dir).unwrap(),
            oc.decision_sha256
        );
    }
}

/// Roadmap § 13 no look-ahead: facts that arrive after the decision — a
/// second demand post, an independent report that would corroborate the
/// news, a backfilled report observed later — change no byte of the cycle;
/// decided later, the same facts do change it.
#[tokio::test]
async fn later_evidence_cannot_change_cycle() {
    let case = load_case("strong_news_weak_demand");
    let (by, _) = build_records(&case.records);
    let f = &by["0000000001-26-000101"];
    let later = |native: &str, src_: Src, published: &str, observed: Option<&str>| {
        let p = t(published);
        let o = observed.map(t).unwrap_or(p + 5 * MIN);
        let mut r = rec(src_, native, p, o, o + MIN);
        if src_.id != Src::FORUM.id {
            r.event_key = f.event_key.clone();
            r = r.with_identity();
        }
        r
    };
    let extra = vec![
        later("p-9", Src::FORUM, "2026-10-05T12:00:01Z", None),
        later("d-9", Src::DAILY, "2026-10-06T08:00:00Z", None),
        // Published before the decision, read only after it (a backfill).
        later(
            "d-8",
            Src::DAILY,
            "2026-10-04T08:00:00Z",
            Some("2026-10-07T08:00:00Z"),
        ),
    ];
    for target in [
        Target::Cycle,
        Target::Replay {
            run_id: "r-later".into(),
        },
    ] {
        let a = Bench::new();
        let (oa, ra) = a.run_case(&case, target.clone()).await;
        let b = Bench::new();
        b.sources.add(&extra);
        let (ob, rb) = b.run_case(&case, target.clone()).await;
        assert_eq!(a.store.snapshot(), b.store.snapshot(), "{target:?}");
        assert_eq!(oa.decision_sha256, ob.decision_sha256);
        // The stages saw the same goal (same packet sha256).
        assert_eq!(*ra.requests.lock().unwrap(), *rb.requests.lock().unwrap());
        let packet = String::from_utf8(b.store.dir(&ob.dir)["packet.json"].clone()).unwrap();
        for r in &extra[..2] {
            assert!(!packet.contains(&r.record_id), "{}", r.native_id);
        }
    }
    // Knowable at a later decision, they do change the week: not vacuous.
    let mut late = case.clone();
    late.decided_at = "2026-10-08T12:00:00Z".parse().unwrap();
    let a = Bench::new();
    let (oa, _) = a.run_case(&late, Target::Cycle).await;
    let b = Bench::new();
    b.sources.add(&extra);
    let (ob, _) = b.run_case(&late, Target::Cycle).await;
    assert_ne!(oa.decision_sha256, ob.decision_sha256);
    assert_ne!(oa.portfolio.held, ob.portfolio.held);
}

/// Roadmap § 13 no silent unknown: an input nobody knows stays `UNKNOWN`
/// through the whole cycle — gate label, next information, memo, episode —
/// and a Critic can make a known input unknown, never the reverse.
#[tokio::test]
async fn missing_value_stays_unknown() {
    let case = load_case("no_candidate_passes");
    let bench = Bench::new();
    let (out, _) = bench.run_case(&case, Target::Cycle).await;
    let price = "economics.revenue.price_per_month";
    let row = out
        .portfolio
        .held
        .iter()
        .find(|r| r.id == "unpriced-automation")
        .unwrap();
    assert!(row.gates.contains(&format!("UNKNOWN_INPUT:{price}")));
    assert!(out.portfolio.next_information.contains(&price.to_string()));
    let files = bench.store.dir(&out.dir);
    let memo = file(&files, MEMO);
    let unknowns = memo.split("## Unknowns").nth(1).unwrap();
    assert!(
        unknowns.contains(price) && unknowns.contains("no public price list"),
        "{unknowns}"
    );
    let e: OpportunityEpisode = from_json(
        file(&files, EPISODES)
            .lines()
            .find(|l| l.contains("\"unpriced-automation\""))
            .unwrap(),
    )
    .unwrap();
    for x in [
        &e.forecast_base.monthly_cash,
        &e.forecast_base.time_adjusted,
    ] {
        assert!(
            matches!(x, Est::Unknown { reason: Some(r) } if r.contains(price)),
            "{x:?}"
        );
    }
    assert_eq!(e.verdict, crate::domain::soe::record::Verdict::Hold);

    // The Critic makes the passing integration's win probability unknown:
    // it holds on it; an `UNKNOWN` never turns back into a number.
    let mut case = load_case("high_ticket_integration");
    let unknown: toml::Value = toml::from_str(
        r#"
target = "erp-integration"
kind = "BASE_RATE"
claim = "no win rate is known for buyers who post on forums"
evidence = []
effect = { kind = "WIDEN", field = "economics.revenue.win_probability", unknown_reason = "no base rate" }
"#,
    )
    .unwrap();
    let back: toml::Value = toml::from_str(
        r#"
target = "erp-integration"
kind = "BASE_RATE"
claim = "assume the usual win rate"
evidence = []
effect = { kind = "WIDEN", field = "economics.revenue.win_probability", low = "9000", base = "9000", high = "9000" }
"#,
    )
    .unwrap();
    case.challenges = vec![unknown, back];
    case.expected.ranked.clear();
    let bench = Bench::new();
    let (out, _) = bench.run_case(&case, Target::Cycle).await;
    let held = &out.portfolio.held[0];
    assert_eq!(held.id, "erp-integration");
    assert!(
        held.gates
            .contains(&"UNKNOWN_INPUT:economics.revenue.win_probability".to_string()),
        "{held:?}"
    );
    let decided = json_file(&bench.store.dir(&out.dir), DECIDED);
    let row = &decided["candidates"][0];
    assert_eq!(row["changes"][0]["after"], "UNKNOWN: no base rate");
    assert!(row["ignored"][0]["why"]
        .as_str()
        .unwrap()
        .contains("cannot make it known"));
}

/// Roadmap § 13 staged exposure: under every weekly budget the cycle's
/// tests fit the profile's owner hours and tranche, and the allocation is
/// exactly what the actions take.
#[tokio::test]
async fn budget_never_exceeds_hours_or_tranche() {
    use crate::domain::soe::allocate::tests::automation;
    use crate::domain::soe::experiment::Stage as XStage;
    use crate::domain::soe::observe::tests::filing;
    use crate::domain::soe::proposal::tests::draft_on;

    let f = filing("0000000001-26-000901", 3);
    let costs = [
        ("0.00", 6),
        ("500.00", 3),
        ("900.00", 2),
        ("400.00", 1),
        ("1500.00", 0),
        ("0.00", 1),
    ];
    let drafts: Vec<String> = costs
        .iter()
        .enumerate()
        .map(|(i, (cash, hours))| {
            let mut o = automation(&format!("c{i}"), &format!("\"{}.00\"", 700 - i * 10));
            o.experiment.as_mut().unwrap().stages = [(*cash, *hours), ("100.00", 1)]
                .iter()
                .enumerate()
                .map(|(n, (c, h))| XStage {
                    name: format!("stage-{n}"),
                    cash: c.parse().unwrap(),
                    recoverable: Minor::ZERO,
                    owner_hours: *h,
                    stop_rule: Some("no signal by the deadline".into()),
                })
                .collect();
            serde_json::to_string(&draft_on(o, &[&f])).unwrap()
        })
        .collect();
    let at = crate::domain::soe::observe::tests::t_ms();
    let mut runs = 0;
    for hours in [1, 3, 6, 9, 10, 12] {
        for tranche in ["100.00", "450.00", "1000.00", "1500.00"] {
            let bench = Bench::new().with_budget(hours, tranche);
            bench.sources.add(std::slice::from_ref(&f));
            let runner = ScriptedRunner::new(bench.store.clone(), drafts.clone(), Vec::new());
            let mut p = bench.params("2026-W41", at, Target::Cycle);
            // `c5` is under way: its next stage (100.00, 1 h) competes too.
            p.active = BTreeMap::from([("c5".to_string(), 1)]);
            let out = bench.cycle(Some(&runner), &p).await.unwrap();
            assert!(runner.refused.lock().unwrap().is_empty());
            let w = &out.portfolio;
            let prof = &bench.profile.record;
            assert!(
                w.allocation.owner_hours <= prof.weekly_owner_hours,
                "{hours} h {tranche}"
            );
            assert!(
                w.allocation.cash <= prof.max_validation_tranche,
                "{hours} h {tranche}"
            );
            let (mut h, mut c) = (0u32, Minor::ZERO);
            for a in w
                .ranked
                .iter()
                .map(|r| &r.action)
                .chain(w.held.iter().map(|r| &r.action))
            {
                match a {
                    PortfolioAction::CheapTest {
                        max_cash,
                        max_hours,
                    } => {
                        assert!(*max_cash <= prof.max_validation_tranche);
                        h += max_hours;
                        c = c.checked_add(*max_cash).unwrap();
                    }
                    PortfolioAction::ContinueActive => {
                        h += 1;
                        c = c.checked_add("100.00".parse().unwrap()).unwrap();
                    }
                    _ => {}
                }
            }
            assert_eq!(
                (h, c),
                (w.allocation.owner_hours, w.allocation.cash),
                "{hours} h {tranche}"
            );
            let inputs = json_file(&bench.store.dir(&out.dir), INPUTS);
            assert_eq!(inputs["budget"]["owner_hours"], hours);
            assert_eq!(inputs["budget"]["cash"], tranche);
            assert_eq!(out.episodes, costs.len());
            runs += 1;
        }
    }
    assert_eq!(runs, 24);
}

/// A runner whose stages run `f` (synchronous tool calls), then succeed.
pub(crate) struct FnRunner<F>(pub F);

#[async_trait]
impl<F: Fn(&StageRequest) + Send + Sync> StageRunner for FnRunner<F> {
    async fn run(&self, req: &StageRequest) -> Result<StageReply> {
        (self.0)(req);
        Ok(StageReply {
            ok: true,
            summary: "probe".into(),
            ..StageReply::default()
        })
    }
}

fn refusal_codes<T>(
    r: Result<std::result::Result<T, Vec<crate::domain::soe::value::ValueError>>>,
) -> Vec<&'static str> {
    match r.unwrap() {
        Ok(_) => Vec::new(),
        Err(e) => e.iter().map(|x| x.code).collect(),
    }
}

/// A frozen cycle never runs again; a claimed one that never froze is not
/// silently reused; neither changes a byte.
#[tokio::test]
async fn frozen_cycle_refused() {
    let case = load_case("strong_news_weak_demand");
    let bench = Bench::new();
    bench.run_case(&case, Target::Cycle).await;
    let before = bench.store.snapshot();
    let (again, _) = bench.try_case(&case, Target::Cycle, |_| {}).await;
    let e = format!("{:#}", again.unwrap_err());
    assert!(
        e.starts_with("cycle_already_frozen: cycles/2026-W41"),
        "{e}"
    );
    assert_eq!(bench.store.snapshot().dirs, before.dirs);
    assert_eq!(bench.store.snapshot().logs, before.logs);

    // A live week decided before the last frozen one would break the chain.
    let p = bench.params("2026-W40", t("2026-09-28T12:00:00Z"), Target::Cycle);
    let e = format!("{:#}", bench.cycle(None, &p).await.unwrap_err());
    assert!(e.starts_with("cycle_out_of_order: cycles/2026-W40"), "{e}");
    assert_eq!(bench.store.snapshot().dirs, before.dirs);
    // A replay of that week is fine: it never touches the chain.
    let p = bench.params(
        "2026-W40",
        t("2026-09-28T12:00:00Z"),
        Target::Replay {
            run_id: "r-w40".into(),
        },
    );
    bench.cycle(None, &p).await.unwrap();

    // An unfinished dir (claimed, never frozen) is refused too.
    let w42 = RunDir::Cycle("2026-W42".into());
    bench.store.claim(&w42).unwrap();
    let p = bench.params("2026-W42", t("2026-10-12T12:00:00Z"), Target::Cycle);
    let e = format!("{:#}", bench.cycle(None, &p).await.unwrap_err());
    assert!(e.starts_with("cycle_unfinished: cycles/2026-W42"), "{e}");

    // A failure after the claim leaves `failed.json`, nothing frozen or logged.
    let bad = Bench::new();
    let mut p = bad.params("2026-W41", t("2026-10-05T12:00:00Z"), Target::Cycle);
    p.forecast_max_weeks = 1;
    let (by, stored) = build_records(&case.records);
    bad.sources.add(&stored);
    let fill = FnRunner(|req: &StageRequest| {
        if req.stage == Stage::Architect {
            // Written before the cap tightened: the cycle re-checks it.
            let mut d: Value = serde_json::from_str(&drafts(&case.proposals, &by)[0]).unwrap();
            d["forecast"] = Value::Array(Vec::new());
            let p = MechanismProposal::stamp(
                "2026-W41.p01",
                "2026-W41",
                serde_json::from_value(d).unwrap(),
                stamp(Stage::Architect, &req.agent, 1),
            )
            .unwrap();
            let mut p = p;
            p.draft.forecast = serde_json::from_value(serde_json::json!([{
                "observable": {"kind": "OPERATOR_RESOLVES", "question": "q"},
                "probability": 5000,
                "resolve_by": "2026-12-31"
            }]))
            .unwrap();
            bad.store.append_proposal(&req.dir, &p).unwrap();
        }
    });
    let e = format!("{:#}", bad.cycle(Some(&fill), &p).await.unwrap_err());
    assert!(e.contains("horizon_too_long"), "{e}");
    let dir = RunDir::Cycle("2026-W41".into());
    assert_eq!(bad.store.status(&dir).unwrap(), RunStatus::Open);
    assert!(bad.store.dir(&dir).contains_key("failed.json"));
    assert!(!bad.store.dir(&dir).contains_key(MANIFEST));
    assert!(bad.store.snapshot().logs.values().all(Vec::is_empty));
}

/// A run stopped between its freeze and its state-log appends (a crash, a
/// shutdown that aborted the job) loses no line: a rerun of the week puts
/// back exactly what was lost — each line once — and is still refused; the
/// next week chains after it instead of past it. A frozen cycle whose line
/// no longer follows the tip is never appended (`chain_broken`).
#[tokio::test]
async fn stopped_after_freeze_resumes_its_log_lines() {
    let case = load_case("weekly_rerun_w41");
    let lose = |bench: &Bench| {
        let mut m = bench.store.mem.lock().unwrap();
        m.logs.get_mut(&StateLog::ForecastLog).unwrap().pop();
        // The stop came in the middle of the episodes.
        m.logs.get_mut(&StateLog::Episodes).unwrap().pop();
    };
    let bench = Bench::new();
    bench.run_case(&case, Target::Cycle).await;
    let whole = bench.store.snapshot().logs;
    assert!(whole[&StateLog::Episodes].len() >= 2, "{whole:?}");
    lose(&bench);
    let s = verify_state(&*bench.store).unwrap();
    assert_eq!(s.log, [("2026-W41".to_string(), LogState::Missing)]);
    // The week's rerun: refused, the lines back — in order, none twice.
    let (again, _) = bench.try_case(&case, Target::Cycle, |_| {}).await;
    let e = format!("{:#}", again.unwrap_err());
    assert!(
        e.starts_with("cycle_already_frozen: cycles/2026-W41"),
        "{e}"
    );
    assert_eq!(bench.store.snapshot().logs, whole);
    assert!(verify_state(&*bench.store).unwrap().ok());

    // The next week instead: it resumes W41 first, then chains after it.
    let bench = Bench::new();
    bench.run_case(&case, Target::Cycle).await;
    lose(&bench);
    let p = bench.params("2026-W42", t("2026-10-12T12:00:00Z"), Target::Cycle);
    bench.cycle(None, &p).await.unwrap();
    let s = verify_state(&*bench.store).unwrap();
    assert!(s.ok(), "{s:?}");
    let ids: Vec<String> = bench
        .store
        .lines(StateLog::ForecastLog)
        .unwrap()
        .iter()
        .map(|l| serde_json::from_str::<LogLine>(l).unwrap().cycle_id)
        .collect();
    assert_eq!(ids, ["2026-W41", "2026-W42"]);
    assert!(
        bench.store.snapshot().logs[&StateLog::Episodes].starts_with(&whole[&StateLog::Episodes])
    );

    // A lost line the chain has moved past is never appended.
    let bench = Bench::new();
    bench.run_case(&case, Target::Cycle).await;
    {
        // An intact one-line chain whose tip is another cycle's line.
        let mut m = bench.store.mem.lock().unwrap();
        let mut f: Forecast = serde_json::from_slice(
            &m.dirs[&RunDir::Cycle("2026-W41".into())][crate::application::soe::cycle::FORECAST],
        )
        .unwrap();
        f.cycle_id = "2026-W40".into();
        let other = crate::domain::soe::forecast::log_line(None, &f).unwrap();
        let log = m.logs.get_mut(&StateLog::ForecastLog).unwrap();
        log.clear();
        log.push(canonical_json(&serde_json::to_value(&other).unwrap()));
    }
    assert!(verify_chain(
        &bench
            .store
            .lines(StateLog::ForecastLog)
            .unwrap()
            .iter()
            .map(|l| serde_json::from_str::<LogLine>(l).unwrap())
            .collect::<Vec<_>>()
    )
    .is_ok());
    let before = bench.store.snapshot().logs;
    let e = format!(
        "{:#}",
        bench
            .cycle(
                None,
                &bench.params("2026-W42", t("2026-10-12T12:00:00Z"), Target::Cycle)
            )
            .await
            .unwrap_err()
    );
    assert!(e.starts_with("chain_broken: "), "{e}");
    assert_eq!(bench.store.snapshot().logs, before);
    assert_eq!(
        bench
            .store
            .status(&RunDir::Cycle("2026-W42".into()))
            .unwrap(),
        RunStatus::Absent
    );
}

/// `runtime.db`'s lease rule in memory (`outbound/runtime_store.rs`
/// `ACQUIRE_SQL`): granted when free, expired or the holder's own; a
/// renewal keeps `acquired_at_ms`. Resource → (holder, acquired, expires).
#[derive(Default)]
struct MemLeases(Mutex<BTreeMap<String, (String, i64, i64)>>);

#[async_trait]
impl RuntimeStore for MemLeases {
    async fn acquire_lease(
        &self,
        resource: &str,
        holder: &str,
        ttl_ms: i64,
        now_ms: i64,
    ) -> Result<RunnerLease> {
        let mut m = self.0.lock().unwrap();
        let row = m.get(resource).cloned();
        let granted = row
            .as_ref()
            .is_none_or(|(h, _, expires)| h == holder || *expires <= now_ms);
        if granted {
            let acquired = match &row {
                Some((h, a, _)) if h == holder => *a,
                _ => now_ms,
            };
            m.insert(resource.into(), (holder.into(), acquired, now_ms + ttl_ms));
        }
        let (current, acquired, expires) = m[resource].clone();
        Ok(RunnerLease {
            resource: resource.into(),
            holder: holder.into(),
            granted,
            current_holder: current,
            acquired_at_ms: acquired,
            expires_at_ms: expires,
        })
    }

    async fn release_lease(&self, resource: &str, holder: &str) -> Result<()> {
        let mut m = self.0.lock().unwrap();
        if m.get(resource).is_some_and(|(h, _, _)| h == holder) {
            m.remove(resource);
        }
        Ok(())
    }

    async fn write_heartbeat(&self, _hb: &Heartbeat) -> Result<()> {
        Ok(())
    }
}

/// `state:soe` held by `me`, as a cycle writes under it (`HeldLeases`, what
/// `OwnerLeases` builds); [`Leased::steal`] gives it to `thief` as though
/// it had expired — a stage outlived the TTL and another process took the
/// SOE state root.
struct Leased {
    store: Arc<MemLeases>,
    held: Arc<HeldLeases>,
}

const STATE_LEASE: &str = "state:soe";

impl Leased {
    async fn new() -> Leased {
        let store = Arc::new(MemLeases::default());
        let mine = store
            .acquire_lease(STATE_LEASE, "me", 60_000, now_ms())
            .await
            .unwrap();
        assert!(mine.granted);
        let held = Arc::new(HeldLeases::new(store.clone(), vec![mine], 60_000));
        Leased { store, held }
    }

    async fn steal(&self) {
        let after_expiry = now_ms() + 120_000;
        let l = self
            .store
            .acquire_lease(STATE_LEASE, "thief", 60_000, after_expiry)
            .await
            .unwrap();
        assert!(l.granted, "{l:?}");
    }
}

/// The scripted stages; once stage `at` has run — or, `hang`, as it starts
/// (that stage never returns) — the lease is stolen and the store kept as
/// it stood then.
struct StealAt<'a> {
    inner: &'a ScriptedRunner,
    lease: &'a Leased,
    at: Stage,
    hang: bool,
    seen: Mutex<Option<Mem>>,
}

impl StealAt<'_> {
    async fn steal(&self) {
        self.lease.steal().await;
        *self.seen.lock().unwrap() = Some(self.inner.store.snapshot());
    }

    fn seen(&self) -> Mem {
        self.seen.lock().unwrap().clone().expect("stolen")
    }
}

#[async_trait]
impl StageRunner for StealAt<'_> {
    async fn run(&self, req: &StageRequest) -> Result<StageReply> {
        if req.stage == self.at && self.hang {
            self.inner.requests.lock().unwrap().push(req.clone());
            self.steal().await;
            return std::future::pending().await;
        }
        let reply = self.inner.run(req).await;
        if req.stage == self.at {
            self.steal().await;
        }
        reply
    }
}

/// `lease`, stolen at the first renewal after `log` got its first line —
/// a loss between two state-log appends.
struct StealAfterLine<'a> {
    lease: &'a Leased,
    store: &'a MemCycleStore,
    log: StateLog,
    stolen: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl Ownership for StealAfterLine<'_> {
    async fn ensure(&self) -> Result<()> {
        let first = !self.store.lines(self.log)?.is_empty()
            && !self.stolen.swap(true, std::sync::atomic::Ordering::SeqCst);
        if first {
            self.lease.steal().await;
        }
        self.lease.held.ensure().await
    }

    async fn lost(&self) -> LeaseLost {
        self.lease.held.lost().await
    }
}

/// Fail-stop on a lost lease (`cycle.rs` module table; review P1): once
/// the SOE state root's lease is another holder's, the cycle calls no
/// further stage, freezes nothing, appends no state-log line, writes no
/// `failed.json` — nothing at all — and fails `lease_lost`. Lost while the
/// Architect ran: no Critic. Lost while the Critic runs (it never returns):
/// the stage is dropped once the renewal task sees the loss. Lost between
/// two state-log lines after the freeze: the lines stop, the next owner's
/// cycle resumes the rest. Lost before the start: nothing is created.
#[tokio::test]
async fn a_lost_lease_stops_the_cycle_writing() {
    let case = load_case("platform_rule_change");
    let w41 = RunDir::Cycle("2026-W41".into());
    let stages = |r: &ScriptedRunner| -> Vec<Stage> {
        r.requests.lock().unwrap().iter().map(|q| q.stage).collect()
    };
    let lost = |e: &anyhow::Error| {
        let msg = format!("{e:#}");
        assert!(
            LeaseLost::of(e).is_some()
                && msg.starts_with("lease_lost: lease `state:soe` lost to `thief`"),
            "{msg}"
        );
    };

    // Lost while the Architect ran.
    let bench = Bench::new();
    let lease = Leased::new().await;
    let (runner, p) = bench.script(&case, Target::Cycle);
    let steal = StealAt {
        inner: &runner,
        lease: &lease,
        at: Stage::Architect,
        hang: false,
        seen: Mutex::new(None),
    };
    let e = bench
        .cycle_under(Some(&steal), &p, &*lease.held)
        .await
        .unwrap_err();
    lost(&e);
    assert_eq!(stages(&runner), [Stage::Architect], "no Critic");
    assert_eq!(
        bench.store.snapshot(),
        steal.seen(),
        "nothing written after"
    );
    let files = bench.store.dir(&w41);
    assert!(files.contains_key(PROPOSALS) && !files.contains_key(FAILED));
    assert_eq!(bench.store.status(&w41).unwrap(), RunStatus::Open);
    assert!(bench.store.snapshot().logs.values().all(Vec::is_empty));

    // Lost while the Critic runs: dropped at the renewal task's next tick.
    let bench = Bench::new();
    let lease = Leased::new().await;
    let (runner, p) = bench.script(&case, Target::Cycle);
    let steal = StealAt {
        inner: &runner,
        lease: &lease,
        at: Stage::Challenge,
        hang: true,
        seen: Mutex::new(None),
    };
    let stopper = Supervisor::new().stopper();
    let timing = LeaseTiming {
        ttl_ms: 60_000,
        renew_ms: 10,
    };
    let keeper = tokio::spawn(keep_lease(
        Arc::clone(&lease.held),
        0,
        timing,
        stopper.clone(),
        stopper.subscribe(),
    ));
    let e = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        bench.cycle_under(Some(&steal), &p, &*lease.held),
    )
    .await
    .expect("the hung stage is dropped")
    .unwrap_err();
    lost(&e);
    keeper.await.unwrap();
    assert!(stopper.cause().unwrap().failed);
    assert_eq!(stages(&runner), [Stage::Architect, Stage::Challenge]);
    assert_eq!(
        bench.store.snapshot(),
        steal.seen(),
        "nothing written after"
    );
    assert_eq!(bench.store.status(&w41).unwrap(), RunStatus::Open);

    // Lost between two state-log lines, after the freeze.
    let bench = Bench::new();
    let lease = Leased::new().await;
    let (runner, p) = bench.script(&case, Target::Cycle);
    let owner = StealAfterLine {
        lease: &lease,
        store: &bench.store,
        log: StateLog::Candidates,
        stolen: Default::default(),
    };
    let e = bench
        .cycle_under(Some(&runner), &p, &owner)
        .await
        .unwrap_err();
    lost(&e);
    assert_eq!(bench.store.status(&w41).unwrap(), RunStatus::Frozen);
    let logs = bench.store.snapshot().logs;
    let n = |log: StateLog| logs.get(&log).map_or(0, Vec::len);
    assert_eq!(
        (
            n(StateLog::Candidates),
            n(StateLog::Episodes),
            n(StateLog::ForecastLog)
        ),
        (1, 0, 0)
    );
    let own: Vec<String> = file(&bench.store.dir(&w41), CANDIDATES)
        .lines()
        .map(String::from)
        .collect();
    assert!(own.len() >= 2, "{own:?}");
    // The next owner — another process's next week — resumes W41 first.
    let next = bench.params("2026-W42", t("2026-10-12T12:00:00Z"), Target::Cycle);
    bench.cycle(None, &next).await.unwrap();
    let s = verify_state(&*bench.store).unwrap();
    assert!(s.ok(), "{s:?}");
    let log = bench.store.lines(StateLog::Candidates).unwrap();
    assert_eq!(
        log[..own.len()],
        own[..],
        "W41's lines, each once, in order"
    );

    // Lost before the cycle starts: nothing is created.
    let bench = Bench::new();
    let lease = Leased::new().await;
    lease.steal().await;
    let p = bench.params("2026-W41", t("2026-10-05T12:00:00Z"), Target::Cycle);
    let e = bench.cycle_under(None, &p, &*lease.held).await.unwrap_err();
    lost(&e);
    assert_eq!(bench.store.snapshot(), Mem::default());
}

/// Fail-soft stages: an Architect that fails writes nothing and the week is
/// a valid `HOLD`; one that fails late keeps what its tools wrote.
#[tokio::test]
async fn architect_failure_holds_not_errors() {
    let case = load_case("strong_news_weak_demand");
    let bench = Bench::new();
    let (out, runner) = bench
        .try_case(&case, Target::Cycle, |r| r.architect = Ending::Err)
        .await;
    let out = out.unwrap();
    assert!(out.portfolio.is_hold() && out.portfolio.held.is_empty());
    assert_eq!(
        out.portfolio.hold_rationale.as_deref(),
        Some("no candidate this week")
    );
    assert_eq!(
        runner.requests.lock().unwrap().len(),
        1,
        "no Critic without a candidate"
    );
    let files = bench.store.dir(&out.dir);
    let stages = json_file(&files, STAGES);
    assert_eq!(stages[0]["outcome"], "FAILED");
    assert!(stages[0]["note"].as_str().unwrap().contains("told to fail"));
    assert_eq!(stages[1]["outcome"], "SKIPPED");
    assert_eq!(stages[1]["note"], "no candidate to challenge");
    let ops: CycleOps = serde_json::from_str(file(&files, OPS)).unwrap();
    assert_eq!(ops.failed_stages, 1);
    // No reply: its tokens are unknown, never 0 (§ 13 no silent unknown).
    assert_eq!(ops.tokens_unknown, [Stage::Architect]);
    assert!(ops.stages[1].tokens_unknown);
    assert!(file(&files, MEMO).contains("| ARCHITECT | `soe-architect` | FAILED |"));
    assert!(verify(&*bench.store, &out.dir).unwrap().ok());
    // … and so is the cost, prices or not.
    let priced = Bench::new();
    let mut runner = ScriptedRunner::new(priced.store.clone(), Vec::new(), Vec::new());
    runner.architect = Ending::Err;
    let mut p = priced.params("2026-W41", t("2026-10-05T12:00:00Z"), Target::Cycle);
    p.token_prices = Some(TokenPrices {
        currency: Currency::Usd,
        prompt_per_million: "3.00".parse().unwrap(),
        completion_per_million: "15.00".parse().unwrap(),
    });
    let out = priced.cycle(Some(&runner), &p).await.unwrap();
    let ops: CycleOps = serde_json::from_str(file(&priced.store.dir(&out.dir), OPS)).unwrap();
    assert_eq!(
        ops.cost,
        Cost::Unknown {
            reason: "tokens unknown: ARCHITECT ended with no reply".into()
        }
    );

    // Late failure: the draft landed before the child failed — decided.
    let bench = Bench::new();
    let (out, _) = bench
        .try_case(&case, Target::Cycle, |r| r.architect = Ending::Failed)
        .await;
    let out = out.unwrap();
    assert_eq!(out.portfolio.held[0].id, "news-automation");
    let ops: CycleOps = serde_json::from_str(file(&bench.store.dir(&out.dir), OPS)).unwrap();
    assert_eq!(ops.failed_stages, 1);
    assert!(
        ops.tokens_unknown.is_empty(),
        "a failed reply reports its tokens"
    );

    // No runner at all (`--no-llm`): both stages skipped, a valid empty week.
    let bench = Bench::new();
    let p = bench.params("2026-W41", t("2026-10-05T12:00:00Z"), Target::Cycle);
    let out = bench.cycle(None, &p).await.unwrap();
    assert!(out.portfolio.is_hold());
    let stages = json_file(&bench.store.dir(&out.dir), STAGES);
    assert_eq!(
        (stages[0]["outcome"].as_str(), stages[1]["outcome"].as_str()),
        (Some("SKIPPED"), Some("SKIPPED"))
    );
}

/// PRD § 12 #5: week 2 re-ranks the old candidates with the new — each
/// once — and leaves week 1's files and log lines exactly as they were.
#[tokio::test]
async fn weekly_rerun_no_duplicates_no_rewrite() {
    let w41 = load_case("weekly_rerun_w41");
    let w42 = load_case("weekly_rerun_w42");
    let one = Bench::new();
    one.run_case(&w41, Target::Cycle).await;
    let two = Bench::new();
    let (out, runner) = two.run_case(&w42, Target::Cycle).await;
    let d41 = RunDir::Cycle("2026-W41".into());
    assert_eq!(one.store.dir(&d41), two.store.dir(&d41), "week 1 rewritten");
    for log in StateLog::ALL {
        let (a, b) = (one.store.lines(log).unwrap(), two.store.lines(log).unwrap());
        assert!(b.starts_with(&a), "{log:?}: week 1's lines changed");
    }
    // Each candidate once; the rejected one is not carried.
    let ids: Vec<&str> = out
        .portfolio
        .ranked
        .iter()
        .map(|r| r.id.as_str())
        .chain(out.portfolio.held.iter().map(|r| r.id.as_str()))
        .chain(out.portfolio.rejected.iter().map(|r| r.id.as_str()))
        .collect();
    assert_eq!(ids, ["steady-automation", "demand-automation"]);
    let files = two.store.dir(&out.dir);
    let decided = json_file(&files, DECIDED);
    let rows: BTreeMap<&str, &Value> = decided["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["candidate"].as_str().unwrap(), r))
        .collect();
    let steady = rows["steady-automation"];
    assert_eq!(
        (steady["carried"].as_bool(), steady["proposal"].as_str()),
        (Some(true), Some("2026-W41.p01"))
    );
    // The carried challenge still applies (conservative merge survives the carry).
    assert_eq!(steady["changes"][0]["challenge"], "2026-W41.c01");
    let demand = rows["demand-automation"];
    assert_eq!(
        (
            demand["carried"].as_bool(),
            demand["opportunity_version"].as_u64()
        ),
        (Some(false), Some(2))
    );
    assert_eq!(decided["superseded"][0]["record"], "2026-W41.p02");
    assert_eq!(decided["superseded"][0]["by"], "2026-W42.p01");
    // Carried verbatim: the W41 record bytes.
    let carried: super::submit::Carried =
        serde_json::from_str(file(&files, "carried.json")).unwrap();
    let w41_props = one.store.proposals(&d41).unwrap();
    assert_eq!(carried.proposals, w41_props[..2].to_vec());
    // Only week 2's own proposal predicts; the log chains to week 1.
    let f: Forecast = from_json(file(&files, FORECAST).trim_end()).unwrap();
    assert_eq!(
        f.items
            .iter()
            .map(|x| x.candidate.as_str())
            .collect::<Vec<_>>(),
        ["demand-automation"]
    );
    let log: Vec<LogLine> = two
        .store
        .lines(StateLog::ForecastLog)
        .unwrap()
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].prev_sha256, log[0].line_sha256);
    assert_eq!(verify_chain(&log), Ok(()));
    // Episodes: three in week 1, two in week 2, never one rewritten.
    assert_eq!(two.store.lines(StateLog::Episodes).unwrap().len(), 5);
    // The Architect saw the carried ids, nothing else of week 1.
    let goal = &runner.requests.lock().unwrap()[0].goal;
    assert!(
        goal.contains("- 2026-W41.p01 steady-automation v1"),
        "{goal}"
    );
    assert!(!goal.contains("rebuild-12k"), "{goal}");
}

/// The models see the cycle's packet only: the goal carries ids and the
/// packet's sha256 — never source text — and a draft citing a record
/// outside the packet (published after the decision) is refused.
#[tokio::test]
async fn architect_sees_only_cycle_packets() {
    let case = load_case("strong_news_weak_demand");
    let bench = Bench::new();
    let later = rec(
        Src::FORUM,
        "p-late",
        t("2026-10-06T09:00:00Z"),
        t("2026-10-06T09:05:00Z"),
        t("2026-10-06T09:06:00Z"),
    );
    bench.sources.add(std::slice::from_ref(&later));
    let (by, _) = build_records(&case.records);
    let mut d: Value = serde_json::from_str(&drafts(&case.proposals, &by)[0]).unwrap();
    d["opportunity"]["id"] = "late-automation".into();
    d["opportunity"]["signals"]
        .as_array_mut()
        .unwrap()
        .push(later.record_id.clone().into());
    let cheat = d.to_string();
    let (out, runner) = bench
        .try_case(&case, Target::Cycle, |r| r.proposals.push(cheat.clone()))
        .await;
    let out = out.unwrap();
    let refused = runner.refused.lock().unwrap().clone();
    assert_eq!(refused.len(), 1);
    assert!(
        refused[0].1[0].starts_with("unsupported_evidence:"),
        "{refused:?}"
    );
    assert!(refused[0].1[0].contains(&later.record_id), "ids in full");
    let files = bench.store.dir(&out.dir);
    assert!(!file(&files, "packet.json").contains(&later.record_id));
    let head = json_file(&files, "head.json");
    let reqs = runner.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    for r in &reqs {
        assert!(r.goal.contains(&format!(
            "packet_sha256: {}",
            head["packet_sha256"].as_str().unwrap()
        )));
        assert!(r.goal.contains("cycle: 2026-W41") && r.goal.contains("run: cycles/2026-W41"));
        // The profile and the generation the stage answers under: part of
        // the stage cache key (`outbound/soe/cache.rs` hashes the goal), so
        // a re-signed profile never replays a stage recorded under the old.
        let g = generation();
        for line in [
            format!("profile_sha256: {}", bench.profile.sha256),
            format!("generation: {} {}", g.id, g.sha256),
        ] {
            assert!(r.goal.lines().any(|l| l == line), "{line} in {}", r.goal);
        }
        for leak in ["example.org", "Other events", "record_id", "@", "p-late"] {
            assert!(!r.goal.contains(leak), "{leak} in {}", r.goal);
        }
    }
    assert_eq!(reqs[0].stage, Stage::Architect);
    assert!(reqs[1].goal.contains("- 2026-W41.p01 news-automation v1"));
    // The packet sha256 in the head is the packet file's.
    assert_eq!(
        head["packet_sha256"].as_str().unwrap(),
        crate::domain::canonical::sha256_hex(file(&files, "packet.json").trim_end())
    );
}

/// A replay writes `replays/<run id>/` only — never `cycles/`, never a
/// state log — reads the `knowable` clock, and keeps its own lines.
#[tokio::test]
async fn replay_writes_replays_never_cycles() {
    let case = load_case("weekly_rerun_w41");
    let bench = Bench::new();
    let (out, _) = bench
        .run_case(
            &case,
            Target::Replay {
                run_id: "r-2026-10-08.1".into(),
            },
        )
        .await;
    let m = bench.store.snapshot();
    assert_eq!(
        m.dirs.keys().cloned().collect::<Vec<_>>(),
        std::slice::from_ref(&out.dir)
    );
    assert!(out.dir.is_replay() && bench.store.cycles().unwrap().is_empty());
    assert!(m.logs.values().all(Vec::is_empty), "{:?}", m.logs);
    let files = bench.store.dir(&out.dir);
    assert_eq!(json_file(&files, "head.json")["mode"], "knowable");
    assert_eq!(lines_of(&files, EPISODES).len(), 3);
    assert_eq!(lines_of(&files, CANDIDATES).len(), 3);
    assert!(!files.contains_key(FORECAST_LINE));
    assert!(verify(&*bench.store, &out.dir).unwrap().ok());
}

/// The tools' writes follow the stage window and the cycle's limits; every
/// refusal is listed with its code, nothing is written.
#[tokio::test]
async fn tool_writes_follow_the_window() {
    let case = load_case("strong_news_weak_demand");
    let (by, stored) = build_records(&case.records);
    let good = drafts(&case.proposals, &by)[0].clone();
    let with = |f: &dyn Fn(&mut Value)| {
        let mut v: Value = serde_json::from_str(&good).unwrap();
        f(&mut v);
        v.to_string()
    };
    let challenge = |target: &str| {
        serde_json::json!({
            "target": target, "kind": "BASE_RATE", "claim": "most such tools churn",
            "evidence": [], "effect": {"kind": "NONE"}
        })
        .to_string()
    };
    let bench = Bench::new();
    bench.sources.add(&stored);
    let seen: Mutex<Vec<(String, Vec<&'static str>)>> = Mutex::new(Vec::new());
    let store = bench.store.clone();
    let probe = FnRunner(|req: &StageRequest| {
        let s = &*store;
        let p = |n| stamp(req.stage, &req.agent, n);
        let log = |what: &str, codes: Vec<&'static str>| {
            seen.lock().unwrap().push((what.to_string(), codes))
        };
        if req.stage == Stage::Architect {
            log(
                "good",
                refusal_codes(submit_proposal(s, &req.dir, &good, p(1))),
            );
            log(
                "again",
                refusal_codes(submit_proposal(s, &req.dir, &good, p(2))),
            );
            let mut other = p(3);
            other.generation = "SOE-G9".into();
            let renamed = with(&|v| v["opportunity"]["id"] = "other-automation".into());
            log(
                "generation",
                refusal_codes(submit_proposal(s, &req.dir, &renamed, other)),
            );
            let computed = with(&|v| v["verdict"] = "PASS".into());
            log(
                "computed",
                refusal_codes(submit_proposal(s, &req.dir, &computed, p(4))),
            );
            let far = with(&|v| {
                v["opportunity"]["id"] = "far-automation".into();
                v["forecast"][0]["resolve_by"] = "2027-06-01".into();
            });
            log(
                "horizon",
                refusal_codes(submit_proposal(s, &req.dir, &far, p(5))),
            );
            log(
                "early challenge",
                refusal_codes(submit_challenge(
                    s,
                    &req.dir,
                    &challenge("news-automation"),
                    p(6),
                )),
            );
        } else {
            log(
                "late proposal",
                refusal_codes(submit_proposal(s, &req.dir, &good, p(1))),
            );
            log(
                "unknown target",
                refusal_codes(submit_challenge(s, &req.dir, &challenge("nobody"), p(2))),
            );
            log(
                "challenge",
                refusal_codes(submit_challenge(
                    s,
                    &req.dir,
                    &challenge("news-automation"),
                    p(3),
                )),
            );
        }
    });
    let p = bench.params("2026-W41", t("2026-10-05T12:00:00Z"), Target::Cycle);
    let out = bench.cycle(Some(&probe), &p).await.unwrap();
    let got: BTreeMap<String, Vec<&'static str>> = seen.lock().unwrap().iter().cloned().collect();
    assert_eq!(got["good"], Vec::<&str>::new());
    assert_eq!(got["again"], [codes::DUPLICATE]);
    assert_eq!(got["generation"], ["generation_mismatch"]);
    assert_eq!(got["computed"], [codes::COMPUTED_FIELD]);
    assert_eq!(got["horizon"], [codes::HORIZON_TOO_LONG]);
    assert_eq!(got["early challenge"], ["stage_closed"]);
    assert_eq!(got["late proposal"], ["stage_closed"]);
    assert_eq!(got["unknown target"], [codes::UNKNOWN_TARGET]);
    assert_eq!(got["challenge"], Vec::<&str>::new());
    let dir = out.dir.clone();
    assert_eq!(bench.store.proposals(&dir).unwrap().len(), 1);
    assert_eq!(bench.store.challenges(&dir).unwrap()[0].id, "2026-W41.c01");
    // After the freeze nothing is accepted.
    assert_eq!(
        refusal_codes(submit_proposal(
            &*bench.store,
            &dir,
            &good,
            stamp(Stage::Architect, "a", 9)
        )),
        ["stage_closed"]
    );
    // The cycle's cap on proposals.
    let capped = Bench::new();
    capped.sources.add(&stored);
    let second = with(&|v| v["opportunity"]["id"] = "second-automation".into());
    let runner = ScriptedRunner::new(capped.store.clone(), vec![good.clone(), second], Vec::new());
    let mut p = capped.params("2026-W41", t("2026-10-05T12:00:00Z"), Target::Cycle);
    p.max_proposals = 1;
    capped.cycle(Some(&runner), &p).await.unwrap();
    let refused = runner.refused.lock().unwrap().clone();
    assert_eq!(refused.len(), 1);
    assert!(
        refused[0].1[0].starts_with("too_many_proposals:"),
        "{refused:?}"
    );
}

/// `verify` names a changed, a removed and an added file.
#[tokio::test]
async fn verify_detects_a_changed_file() {
    let case = load_case("strong_news_weak_demand");
    let bench = Bench::new();
    let (out, _) = bench.run_case(&case, Target::Cycle).await;
    bench
        .store
        .tamper(&out.dir, PORTFOLIO, b"{\"ranked\":[\"x\"]}\n");
    bench.store.tamper(&out.dir, "extra.json", b"{}\n");
    bench
        .store
        .mem
        .lock()
        .unwrap()
        .dirs
        .get_mut(&out.dir)
        .unwrap()
        .remove(MEMO);
    let v = verify(&*bench.store, &out.dir).unwrap();
    assert!(!v.ok());
    let bad: BTreeMap<&str, FileCheck> = v
        .files
        .iter()
        .filter(|(_, c)| *c != FileCheck::Match)
        .map(|(n, c)| (n.as_str(), *c))
        .collect();
    assert_eq!(
        bad,
        BTreeMap::from([
            ("extra.json", FileCheck::Extra),
            (MEMO, FileCheck::Absent),
            (PORTFOLIO, FileCheck::Mismatch),
        ])
    );
    // A run never frozen has no manifest.
    let open = RunDir::Replay("r-open".into());
    bench.store.claim(&open).unwrap();
    assert_eq!(verify(&*bench.store, &open).unwrap().manifest_sha256, None);
}
