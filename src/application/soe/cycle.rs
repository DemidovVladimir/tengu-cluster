//! The weekly cycle (O3; PRD § 8 steps 1–7 and 10; roadmap O3, O4): one run
//! of Observe → Architect → Challenge → Allocate → Report over one run dir
//! of the private SOE state, then frozen. The models write data through
//! their tools (`submit.rs`); every figure, gate, rank and action is the
//! pure domain's (`domain::soe::allocate::decide_week`).
//!
//! | Step | Writes (run dir) | Rule |
//! |---|---|---|
//! | Refuse | — | a frozen run (`cycle_already_frozen`), one claimed and never frozen (`cycle_unfinished`), an unsigned profile, a bad generation pin, agent or limit, a broken forecast log (live) — before anything is created |
//! | Observe | `head.json`, `packet.json`, `profile.toml` | `application::sources::evidence_as_of` at `decided_at` — the `captured` clock for a live cycle (what was read by then), `knowable` for a replay; the packet is the stages' only fact input |
//! | Carry | `carried.json` | live only: the previous frozen cycle's candidates outside its rejected list (`REJECT`, `REPRICE`) — verbatim records, never rewritten — and the challenges on them, each re-checked against this packet with its forecast set aside (it stays frozen in its own cycle); one that fails is dropped, with why |
//! | Architect | `phase-propose.json` + its tools' `proposals.jsonl` | `StageRunner` with [`architect_goal`]: run, cycle, decision time, the packet's sha256, carried ids — never source text; no runner ⇒ skipped |
//! | Challenge | `phase-challenge.json` + `challenges.jsonl` | the Critic on the week's candidates ([`critic_goal`]); no runner or no candidate ⇒ skipped |
//! | Decide | `phase-closed.json`, `decided.json`, `portfolio.json` | new + carried proposals (a re-proposed carried candidate is superseded) and challenges (a carried one whose target or field is gone is dropped) → `decide_week` with the active map |
//! | Report | `memo.md`, `forecast.json`, `forecast-line.json` (live), `inputs.json`, `stages.json`, `ops.json`, `candidates.jsonl`, `episodes.jsonl` | `render_memo` + the cycle section; the new proposals' forecasts frozen at `decided_at`; the cycle's identity (packet, proposals, challenges, profile, policy = the rank order, generation, active map, budget); one candidate event and one `OpportunityEpisode` per decided candidate (C20) |
//! | Freeze | `MANIFEST.json`, read-only | `freeze::freeze` |
//! | Learn | live only: the state logs `candidates.jsonl`, `episodes.jsonl`, `forecast-log.jsonl` | the run dir's lines appended (a replay touches no state log) |
//!
//! | Rule | Value |
//! |---|---|
//! | Fail-soft | a stage that fails (`Err` or `ok = false`) is recorded; the cycle goes on with what its tools wrote — a `HOLD` week is a valid answer |
//! | Fail-hard | an IO or domain error after the claim writes `failed.json` and returns `Err`: nothing frozen or logged; the dir stays (`cycle_unfinished` on a rerun) |
//! | Reproducible | every file but `ops.json` is a function of the inputs — canonical JSON, the injected clock only in `ops.json`, ids by append order, forecasts frozen at `decided_at`: the same packet, profile, generation and stage records give the same bytes (`freeze::decision_sha256`) |
//! | No look-ahead | nothing after `decided_at` reaches a stage or the decision: the packet's clock cuts it; carried records are re-checked against it |
//! | No side effect | the cycle writes the SOE state only; no contact, spend, publish or deploy exists |

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::freeze::{decision_sha256, freeze, OPS};
use super::submit::{
    self, json_line, open_phase, Carried, Dropped, GenerationPin, Phase, RunHead, CARRIED, HEAD,
    HEAD_SCHEMA, PACKET,
};
use super::{CYCLE_ALREADY_FROZEN, CYCLE_UNFINISHED};
use crate::application::sources::{evidence_as_of, AsOfRequest};
use crate::config::sources::SourcesConfig;
use crate::domain::canonical::{canonical_json, canonical_sha256, sha256_hex};
use crate::domain::lineage::episode::{DecisionQuality, ExecutionQuality, OutcomeQuality, Quality};
use crate::domain::lineage::value::Time;
use crate::domain::soe::allocate::{decide_week, Budget, Decided, Week, WeekInput};
use crate::domain::soe::challenge::Challenge;
use crate::domain::soe::economics::{Metric, ECONOMICS_VERSION};
use crate::domain::soe::episode::{Actual, Forecast as EpisodeForecast, OpportunityEpisode};
use crate::domain::soe::forecast::{
    freeze as freeze_forecast, horizon_problems, log_line, verify_chain, Forecast, ForecastItem,
    LogLine,
};
use crate::domain::soe::memo::{render_memo, MemoInput};
use crate::domain::soe::observe::EvidenceIndex;
use crate::domain::soe::opportunity::Opportunity;
use crate::domain::soe::ops::{cycle_ops, Stage, StageRun, TokenPrices};
use crate::domain::soe::portfolio::{IsoWeek, PortfolioAction, WeeklyPortfolio};
use crate::domain::soe::profile::{OperatorProfile, RankKey};
use crate::domain::soe::proposal::MechanismProposal;
use crate::domain::soe::rank::{KeyValue, WeekHead};
use crate::domain::soe::record::{validate, Tier};
use crate::domain::soe::value::{Est, Minor, SchemaTag, ValueError};
use crate::domain::source::{fence_untrusted, AsOfMode, EvidencePacket};
use crate::ports::clock::Clock;
use crate::ports::soe::{
    CycleStore, RunDir, RunStatus, StageReply, StageRequest, StageRunner, StateLog,
};
use crate::ports::source_store::SourceStore;

/// The profile file, copied into the run dir.
pub(crate) const PROFILE: &str = "profile.toml";
pub(crate) const DECIDED: &str = "decided.json";
pub(crate) const PORTFOLIO: &str = "portfolio.json";
pub(crate) const MEMO: &str = "memo.md";
pub(crate) const FORECAST: &str = "forecast.json";
/// Live: the forecast-log line this cycle appends (prev hash included).
pub(crate) const FORECAST_LINE: &str = "forecast-line.json";
pub(crate) const INPUTS: &str = "inputs.json";
pub(crate) const STAGES: &str = "stages.json";
/// The run's candidate events and episodes (live: also appended to the state logs).
pub(crate) const CANDIDATES: &str = "candidates.jsonl";
pub(crate) const EPISODES: &str = "episodes.jsonl";
/// Written when a claimed run fails (never frozen).
pub(crate) const FAILED: &str = "failed.json";

const WEEK_MS: i64 = 7 * 86_400_000;

/// The signed profile in force, with its file.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProfileIn<'a> {
    pub record: &'a OperatorProfile,
    /// `toml_digest` of `text` (`config::soe::Loaded::sha256`).
    pub sha256: &'a str,
    /// The file's text, kept with the run.
    pub text: &'a str,
}

/// What a cycle reads and writes through.
#[derive(Clone, Copy)]
pub(crate) struct CycleEnv<'a> {
    /// `None` = no source store yet: an empty packet.
    pub sources: Option<&'a dyn SourceStore>,
    pub registry: &'a SourcesConfig,
    pub store: &'a dyn CycleStore,
    /// `None` = no model stage (`--no-llm`): the cycle decides what is carried.
    pub runner: Option<&'a dyn StageRunner>,
    pub clock: &'a dyn Clock,
    pub profile: ProfileIn<'a>,
}

/// Live cycle or replay (module table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// `cycles/<cycle id>/` + the state logs.
    Cycle,
    /// `replays/<run id>/` only.
    Replay { run_id: String },
}

impl Target {
    pub(crate) fn dir(&self, cycle_id: &str) -> RunDir {
        match self {
            Target::Cycle => RunDir::Cycle(cycle_id.to_string()),
            Target::Replay { run_id } => RunDir::Replay(run_id.clone()),
        }
    }
}

/// One cycle's parameters (the caller takes them from `[soe]`; no default here).
#[derive(Debug, Clone)]
pub(crate) struct CycleParams {
    pub target: Target,
    /// The cycle id is its text (`2026-W41`).
    pub week: IsoWeek,
    pub decided_at_ms: i64,
    pub generation: GenerationPin,
    pub architect: String,
    pub critic: String,
    pub max_proposals: usize,
    pub forecast_max_weeks: u32,
    /// Active candidates: opportunity id → its next experiment stage.
    pub active: BTreeMap<String, usize>,
    /// `None` ⇒ the cost is `UNKNOWN`.
    pub token_prices: Option<TokenPrices>,
}

/// What a finished cycle answers.
#[derive(Debug, Clone)]
pub(crate) struct CycleOutcome {
    pub dir: RunDir,
    pub cycle_id: String,
    pub portfolio: WeeklyPortfolio,
    /// `inputs.json` `inputs_sha256`: the cycle's identity.
    pub inputs_sha256: String,
    /// `freeze::decision_sha256`.
    pub decision_sha256: String,
    pub manifest_sha256: String,
    pub episodes: usize,
    pub stages: Vec<StageRun>,
    /// The decided week (`portfolio` is its portfolio): what replay scoring reads.
    pub week: Week,
}

/// One model stage as `stages.json` keeps it (latency is `ops.json`'s).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct StageLine {
    stage: Stage,
    agent: String,
    /// `OK` · `FAILED` · `SKIPPED`.
    outcome: &'static str,
    /// Why it failed (the runner's error) or was skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    /// The agent's summary — model text.
    summary: String,
    models: Vec<String>,
}

impl StageLine {
    fn skipped(stage: Stage, agent: &str, why: &str) -> StageLine {
        StageLine {
            stage,
            agent: agent.to_string(),
            outcome: "SKIPPED",
            note: Some(why.to_string()),
            summary: String::new(),
            models: Vec::new(),
        }
    }
}

/// `decided.json` row, as the next cycle reads it back.
#[derive(Debug, Clone, Deserialize)]
struct DecidedRead {
    candidates: Vec<DecidedRowRead>,
}

#[derive(Debug, Clone, Deserialize)]
struct DecidedRowRead {
    proposal: String,
    list: String,
}

fn elapsed(clock: &dyn Clock, since: i64) -> u64 {
    u64::try_from(clock.now_ms().saturating_sub(since)).unwrap_or(0)
}

/// The rank order's identity (the public variant would carry only this, C4).
pub(crate) fn policy_sha256(order: &[RankKey]) -> String {
    canonical_sha256(&json!({ "rank_order": order }))
}

fn record_sha256<T: Serialize>(r: &T) -> Result<String> {
    Ok(canonical_sha256(&serde_json::to_value(r)?))
}

/// The Architect's user turn (module table): ids and hashes only.
pub(crate) fn architect_goal(h: &RunHead, records: usize, carried: &Carried) -> String {
    let mut g = format!(
        "soe_stage: ARCHITECT\nrun: {}\ncycle: {}\ndecided_at: {}\npacket_sha256: {}\npacket_records: {records}\nmax_proposals: {}\nforecast_max_weeks: {}\n",
        h.run,
        h.cycle_id,
        h.decided_at(),
        h.packet_sha256,
        h.max_proposals,
        h.forecast_max_weeks
    );
    if carried.proposals.is_empty() {
        g.push_str("carried: none\n");
    } else {
        let _ = writeln!(
            g,
            "carried (from {}; re-propose one to replace it):",
            carried.from.as_deref().unwrap_or("-")
        );
        for p in &carried.proposals {
            let o = p.opportunity();
            let _ = writeln!(g, "- {} {} v{}", p.id, o.id, o.version);
        }
    }
    g
}

/// The Critic's user turn: ids and hashes only.
pub(crate) fn critic_goal(h: &RunHead, candidates: &[MechanismProposal]) -> String {
    let mut g = format!(
        "soe_stage: CHALLENGE\nrun: {}\ncycle: {}\ndecided_at: {}\npacket_sha256: {}\ncandidates:\n",
        h.run,
        h.cycle_id,
        h.decided_at(),
        h.packet_sha256
    );
    for p in candidates {
        let o = p.opportunity();
        let _ = writeln!(g, "- {} {} v{}", p.id, o.id, o.version);
    }
    g
}

async fn run_stage(
    runner: &dyn StageRunner,
    clock: &dyn Clock,
    req: StageRequest,
) -> (StageRun, StageLine) {
    let t = clock.now_ms();
    let reply = match runner.run(&req).await {
        Ok(r) => r,
        Err(e) => StageReply {
            ok: false,
            error: Some(format!("{e:#}")),
            latency_ms: elapsed(clock, t),
            ..StageReply::default()
        },
    };
    let sum = |f: fn(&crate::domain::metrics::MetricsRecord) -> u32| -> u64 {
        reply.metrics.iter().map(|m| u64::from(f(m))).sum()
    };
    let models: BTreeSet<String> = reply.metrics.iter().map(|m| m.model.clone()).collect();
    let run = StageRun {
        stage: req.stage,
        agent: Some(req.agent.clone()),
        latency_ms: reply.latency_ms,
        prompt_tokens: sum(|m| m.prompt_tokens),
        completion_tokens: sum(|m| m.completion_tokens),
        ok: reply.ok,
    };
    let line = StageLine {
        stage: req.stage,
        agent: req.agent,
        outcome: if reply.ok { "OK" } else { "FAILED" },
        note: reply.error,
        summary: reply.summary,
        models: models.into_iter().collect(),
    };
    (run, line)
}

/// The latest frozen cycle before `cycle_id`.
fn previous_cycle(store: &dyn CycleStore, cycle_id: &str) -> Result<Option<String>> {
    let mut prev = None;
    for c in store.cycles()? {
        if c.as_str() < cycle_id && store.status(&RunDir::Cycle(c.clone()))? == RunStatus::Frozen {
            prev = Some(c);
        }
    }
    Ok(prev)
}

/// Module table: carry-forward from the previous frozen cycle.
fn carry(
    store: &dyn CycleStore,
    cycle_id: &str,
    index: &EvidenceIndex,
    at: &Time,
) -> Result<Carried> {
    let Some(from) = previous_cycle(store, cycle_id)? else {
        return Ok(Carried::default());
    };
    let dir = RunDir::Cycle(from.clone());
    let decided: DecidedRead = submit::read_json(store, &dir, DECIDED)?
        .with_context(|| format!("{dir}/{DECIDED}: missing in a frozen cycle"))?;
    let keep: BTreeSet<String> = decided
        .candidates
        .into_iter()
        .filter(|r| r.list != "REJECTED")
        .map(|r| r.proposal)
        .collect();
    let before = submit::carried(store, &dir)?;
    let mut pool = store.proposals(&dir)?;
    pool.extend(before.proposals);
    let mut challenges = store.challenges(&dir)?;
    challenges.extend(before.challenges);
    let mut out = Carried {
        from: Some(from),
        ..Carried::default()
    };
    let why = |e: Vec<ValueError>| -> Vec<String> { e.iter().map(ToString::to_string).collect() };
    for p in pool.into_iter().filter(|p| keep.contains(&p.id)) {
        let mut probe = p.clone();
        probe.draft.forecast.clear();
        match probe.draft.check(index, at) {
            Ok(()) => out.proposals.push(p),
            Err(e) => out.dropped.push(Dropped {
                kind: "PROPOSAL".into(),
                record: p.id.clone(),
                candidate: p.opportunity().id.clone(),
                why: why(e),
            }),
        }
    }
    let targets: Vec<&Opportunity> = out.proposals.iter().map(|p| p.opportunity()).collect();
    let mut kept = Vec::new();
    for c in challenges
        .into_iter()
        .filter(|c| targets.iter().any(|o| o.id == c.draft.target))
    {
        match c.draft.check(index, &targets) {
            Ok(()) => kept.push(c),
            Err(e) => out.dropped.push(Dropped {
                kind: "CHALLENGE".into(),
                record: c.id.clone(),
                candidate: c.draft.target.clone(),
                why: why(e),
            }),
        }
    }
    out.challenges = kept;
    out.proposals.sort_by(|a, b| a.id.cmp(&b.id));
    out.challenges.sort_by(|a, b| a.id.cmp(&b.id));
    out.dropped
        .sort_by(|a, b| (&a.kind, &a.record).cmp(&(&b.kind, &b.record)));
    Ok(out)
}

/// Sources with a failed fetch in the week before the decision.
fn source_failures(packet: &EvidencePacket, at_ms: i64) -> Vec<String> {
    packet
        .freshness
        .iter()
        .filter(|f| {
            f.failed
                .iter()
                .any(|c| c.fetched_ms <= at_ms && c.fetched_ms > at_ms - WEEK_MS)
        })
        .map(|f| f.source_id.clone())
        .collect()
}

/// Where a candidate landed: list, rank, action.
fn placement<'a>(
    p: &'a WeeklyPortfolio,
    id: &str,
) -> Result<(&'static str, Option<u32>, &'a PortfolioAction)> {
    if let Some(r) = p.ranked.iter().find(|r| r.id == id) {
        return Ok(("RANKED", Some(r.rank), &r.action));
    }
    for (list, rows) in [("HELD", &p.held), ("REJECTED", &p.rejected)] {
        if let Some(r) = rows.iter().find(|r| r.id == id) {
            return Ok((list, None, &r.action));
        }
    }
    bail!("candidate `{id}` is in no list of the portfolio")
}

fn est_of<T: Copy>(m: &Metric<T>) -> Est<T> {
    match m {
        Metric::Known(v) => Est::point(*v),
        Metric::Unknown { fields } => Est::unknown(format!("unknown: {}", fields.join(", "))),
    }
}

/// One `OpportunityEpisode` per decided candidate (C20; module table).
fn episode(
    n: usize,
    d: &Decided,
    head: &WeekHead,
    why: &str,
    action: &PortfolioAction,
) -> Result<OpportunityEpisode> {
    let a = &d.assessment;
    let base = &a.scenarios.base;
    let shadow = "shadow cycle: no action ran";
    let e = OpportunityEpisode {
        schema: SchemaTag::v1("opportunity_episode").map_err(|e| anyhow!("{e}"))?,
        id: format!("{}.e{n:02}", head.id),
        version: 1,
        opportunity: a.id.clone(),
        opportunity_version: a.version,
        profile_sha256: head.profile_sha256.clone(),
        decided_at: head.as_of,
        verdict: a.verdict.verdict,
        currency: head.currency,
        actions: Vec::new(),
        spend: Minor::ZERO,
        owner_hours: 0,
        forecast_base: EpisodeForecast {
            monthly_cash: est_of(&base.monthly_cash_contribution),
            time_adjusted: est_of(&base.time_adjusted_contribution),
            owner_hours: est_of(&base.owner_hours_per_month),
        },
        actual: Actual {
            monthly_cash: Est::unknown(shadow),
            owner_hours: Est::unknown(shadow),
        },
        surprise: None,
        evidence_strength: match a.keys.get(&RankKey::EvidenceConfidence) {
            Some(KeyValue::Tier(t)) => *t,
            _ => Tier::Unknown,
        },
        quality: Quality {
            decision: DecisionQuality::Unknown,
            decision_note: format!("{}: {why}", action.kind()),
            execution: ExecutionQuality::NotApplicable,
            execution_note: None,
            outcome: OutcomeQuality::Unknown,
            attribution: Vec::new(),
        },
        lesson: None,
    };
    validate(&e).map_err(|errs| anyhow!("episode `{}`: {errs:?}", e.id))?;
    Ok(e)
}

/// `ARCHITECT`, as the records spell it.
fn stage_name(s: Stage) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// The memo's last section: stages, carried, superseded, dropped.
fn cycle_section(
    lines: &[StageLine],
    carried: &Carried,
    superseded: &[(String, String, String)],
    dropped: &[Dropped],
    active_missing: &[String],
) -> String {
    let mut out =
        String::from("\n## Cycle\n\n| Stage | Agent | Outcome | Note |\n|---|---|---|---|\n");
    for l in lines {
        let note = l
            .note
            .as_deref()
            .map(|e| {
                cell(&fence_untrusted(
                    &format!("stage-{}", stage_name(l.stage)),
                    "note",
                    e,
                ))
            })
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "| {} | `{}` | {} | {note} |",
            stage_name(l.stage),
            l.agent,
            l.outcome
        );
    }
    let _ = writeln!(out);
    match &carried.from {
        None => {
            let _ = writeln!(out, "Carried: nothing (no earlier frozen cycle).");
        }
        Some(from) => {
            let ids: Vec<String> = carried
                .proposals
                .iter()
                .map(|p| format!("`{}` (`{}`)", p.id, p.opportunity().id))
                .collect();
            let _ = writeln!(
                out,
                "Carried from `{from}`: {}.",
                if ids.is_empty() {
                    "nothing".to_string()
                } else {
                    ids.join(", ")
                }
            );
        }
    }
    for (old, new, opp) in superseded {
        let _ = writeln!(out, "\nSuperseded: `{old}` → `{new}` (`{opp}`).");
    }
    for d in dropped {
        let _ = writeln!(
            out,
            "\nDropped {} `{}` (`{}`): {}",
            d.kind.to_lowercase(),
            d.record,
            d.candidate,
            cell(&d.why.join("; "))
        );
    }
    for id in active_missing {
        let _ = writeln!(out, "\nActive `{id}`: no proposal this week.");
    }
    out
}

/// Module table: run one cycle to its frozen end.
pub(crate) async fn run_cycle(env: &CycleEnv<'_>, p: &CycleParams) -> Result<CycleOutcome> {
    let cycle_id = p.week.to_string();
    let dir = p.target.dir(&cycle_id);
    let prev_line = preflight(env, p, &dir)?;
    let t0 = env.clock.now_ms();
    let mode = if dir.is_replay() {
        AsOfMode::Knowable
    } else {
        AsOfMode::Captured
    };
    let packet = evidence_as_of(
        env.sources,
        env.registry,
        &AsOfRequest {
            at_ms: p.decided_at_ms,
            mode,
            source: None,
            entity: None,
            event_key: None,
            published_from_ms: None,
        },
    )
    .await?;
    let index = EvidenceIndex::of(&packet);
    let at = Time::At(p.decided_at_ms);
    let carried = if dir.is_replay() {
        Carried::default()
    } else {
        carry(env.store, &cycle_id, &index, &at)?
    };
    let head = RunHead {
        schema: HEAD_SCHEMA.into(),
        run: dir.to_string(),
        cycle_id: cycle_id.clone(),
        decided_at_ms: p.decided_at_ms,
        mode,
        packet_sha256: sha256_hex(&canonical_json(&serde_json::to_value(&packet)?)),
        generation: p.generation.clone(),
        architect: p.architect.clone(),
        critic: p.critic.clone(),
        max_proposals: p.max_proposals,
        forecast_max_weeks: p.forecast_max_weeks,
    };
    let observe = StageRun {
        stage: Stage::Observe,
        agent: None,
        latency_ms: elapsed(env.clock, t0),
        prompt_tokens: 0,
        completion_tokens: 0,
        ok: true,
    };
    env.store
        .claim(&dir)
        .with_context(|| format!("claim {dir} in {}", env.store.root_display()))?;
    let claimed = Claimed {
        env,
        p,
        dir: dir.clone(),
        head,
        packet,
        index,
        carried,
        prev_line,
    };
    match claimed.run(observe).await {
        Ok(o) => Ok(o),
        Err(e) => {
            if let Ok(b) = json_line(&json!({"run": dir.to_string(), "error": format!("{e:#}")})) {
                let _ = env.store.write(&dir, FAILED, &b);
            }
            Err(e)
        }
    }
}

/// The refusals before anything is created; the forecast log's last line (live).
fn preflight(env: &CycleEnv, p: &CycleParams, dir: &RunDir) -> Result<Option<LogLine>> {
    let mut problems = p.generation.problems();
    if !env.profile.record.is_signed() {
        problems.push(format!(
            "{}: profile `{}` is not signed",
            crate::domain::soe::value::codes::OPERATOR_PROFILE_UNSIGNED,
            env.profile.record.id
        ));
    }
    for (what, agent) in [("architect", &p.architect), ("critic", &p.critic)] {
        if agent.trim().is_empty() {
            problems.push(format!("no {what} agent"));
        }
    }
    if p.architect == p.critic {
        problems.push(format!(
            "architect and critic are one agent `{}` — the critic must not judge its own proposals",
            p.architect
        ));
    }
    if p.max_proposals == 0 || p.forecast_max_weeks == 0 {
        problems.push("max_proposals and forecast_max_weeks must be ≥ 1".into());
    }
    if !problems.is_empty() {
        bail!("cycle {dir} refused: {}", problems.join("; "));
    }
    match env.store.status(dir)? {
        RunStatus::Frozen => bail!(
            "{CYCLE_ALREADY_FROZEN}: {dir} in {} is frozen — a cycle runs once; replay it under replays/",
            env.store.root_display()
        ),
        RunStatus::Open => bail!(
            "{CYCLE_UNFINISHED}: {dir} in {} was claimed and never frozen (see its {FAILED}) — move it aside to rerun",
            env.store.root_display()
        ),
        RunStatus::Absent => {}
    }
    if dir.is_replay() {
        return Ok(None);
    }
    let lines: Vec<LogLine> = env
        .store
        .lines(StateLog::ForecastLog)?
        .iter()
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l)
                .with_context(|| format!("{} line {}", StateLog::ForecastLog.file_name(), i + 1))
        })
        .collect::<Result<_>>()?;
    if let Err(e) = verify_chain(&lines) {
        bail!(
            "{}: {} — nothing appends to a broken chain: {}",
            crate::domain::soe::value::codes::CHAIN_BROKEN,
            StateLog::ForecastLog.file_name(),
            e.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    if lines.iter().any(|l| l.cycle_id == dir.id()) {
        bail!(
            "{CYCLE_ALREADY_FROZEN}: {} already holds cycle {}",
            StateLog::ForecastLog.file_name(),
            dir.id()
        );
    }
    Ok(lines.last().cloned())
}

/// A candidate event's schema (`candidates.jsonl`).
const EVENT_SCHEMA: &str = "soe.candidate_event/1";

/// `v` as one canonical JSON text (a state-log line).
fn canonical_line<T: Serialize>(v: &T) -> Result<String> {
    Ok(canonical_json(&serde_json::to_value(v)?))
}

fn joined(errs: &[ValueError]) -> String {
    errs.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// A stage the cycle runs itself (no model, no tokens).
fn local_run(stage: Stage, latency_ms: u64) -> StageRun {
    StageRun {
        stage,
        agent: None,
        latency_ms,
        prompt_tokens: 0,
        completion_tokens: 0,
        ok: true,
    }
}

/// The run's stages: `ops.json` runs and `stages.json` lines.
struct Stages {
    runs: Vec<StageRun>,
    lines: Vec<StageLine>,
}

/// What the week decides over (module table: Decide).
struct WeekRecords {
    proposals: Vec<MechanismProposal>,
    challenges: Vec<Challenge>,
    /// `(carried record, the new proposal, opportunity)`.
    superseded: Vec<(String, String, String)>,
    dropped: Vec<Dropped>,
}

/// What a decided week teaches (module table: Report, Learn).
struct Learned {
    rows: Vec<Value>,
    events: Vec<Value>,
    episodes: Vec<OpportunityEpisode>,
    active_missing: Vec<String>,
}

/// A claimed run: everything after the claim (module table).
struct Claimed<'a> {
    env: &'a CycleEnv<'a>,
    p: &'a CycleParams,
    dir: RunDir,
    head: RunHead,
    packet: EvidencePacket,
    index: EvidenceIndex,
    carried: Carried,
    prev_line: Option<LogLine>,
}

impl Claimed<'_> {
    fn write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        self.env.store.write(&self.dir, name, bytes)
    }

    fn write_json<T: Serialize>(&self, name: &str, v: &T) -> Result<()> {
        self.write(name, &json_line(v)?)
    }

    fn write_lines<T: Serialize>(&self, name: &str, rows: &[T]) -> Result<()> {
        let mut b = Vec::new();
        for r in rows {
            b.extend(json_line(r)?);
        }
        self.write(name, &b)
    }

    async fn run(self, observe: StageRun) -> Result<CycleOutcome> {
        let (env, p, dir) = (self.env, self.p, &self.dir);
        self.write_json(HEAD, &self.head)?;
        self.write_json(PACKET, &self.packet)?;
        self.write(PROFILE, env.profile.text.as_bytes())?;
        self.write_json(CARRIED, &self.carried)?;
        let mut stages = Stages {
            runs: vec![observe],
            lines: Vec::new(),
        };
        self.model_stages(&mut stages).await?;

        let t = env.clock.now_ms();
        let records = self.assemble()?;
        let head = WeekHead {
            id: self.head.cycle_id.clone(),
            week: p.week,
            as_of: self.head.decided_at(),
            currency: env.profile.record.currency,
            profile_sha256: env.profile.sha256.to_string(),
        };
        let week = decide_week(&WeekInput {
            head: &head,
            packet: &self.packet,
            proposals: &records.proposals,
            challenges: &records.challenges,
            active: &p.active,
            profile: env.profile.record,
        })
        .map_err(|e| anyhow!("decide_week refused the cycle: {}", joined(&e)))?;
        stages
            .runs
            .push(local_run(Stage::Allocate, elapsed(env.clock, t)));

        let t = env.clock.now_ms();
        let learned = self.learn(&head, &week, &records)?;
        let forecast = self.forecast(&week, &records)?;
        let (inputs, inputs_sha256) = self.inputs(&week, &records)?;
        let memo = render_memo(&MemoInput {
            week: &week,
            proposals: &records.proposals,
            challenges: &records.challenges,
        }) + &cycle_section(
            &stages.lines,
            &self.carried,
            &records.superseded,
            &records.dropped,
            &learned.active_missing,
        );
        stages
            .runs
            .push(local_run(Stage::Report, elapsed(env.clock, t)));
        let ops = cycle_ops(
            &self.head.cycle_id,
            stages.runs.clone(),
            source_failures(&self.packet, self.head.decided_at_ms),
            p.token_prices.as_ref(),
        )
        .map_err(|e| anyhow!("ops: {e}"))?;

        self.write_json(DECIDED, &self.decided(&records, &learned))?;
        self.write_json(PORTFOLIO, &week.portfolio)?;
        self.write(MEMO, memo.as_bytes())?;
        self.write_json(FORECAST, &forecast)?;
        self.write_json(INPUTS, &inputs)?;
        self.write_json(STAGES, &stages.lines)?;
        self.write_json(OPS, &ops)?;
        self.write_lines(CANDIDATES, &learned.events)?;
        self.write_lines(EPISODES, &learned.episodes)?;
        let line = match dir {
            RunDir::Cycle(_) => {
                let l = log_line(self.prev_line.as_ref(), &forecast).map_err(|e| anyhow!("{e}"))?;
                self.write_json(FORECAST_LINE, &l)?;
                Some(l)
            }
            RunDir::Replay(_) | RunDir::Review(_) => None,
        };

        // Freeze, then learn (live only; the lines are the run dir's).
        let manifest_sha256 = freeze(env.store, dir)?;
        let decision = decision_sha256(env.store, dir)?;
        if let Some(l) = line {
            for e in &learned.events {
                env.store
                    .append_line(StateLog::Candidates, &canonical_line(e)?)?;
            }
            for e in &learned.episodes {
                env.store
                    .append_line(StateLog::Episodes, &canonical_line(e)?)?;
            }
            env.store
                .append_line(StateLog::ForecastLog, &canonical_line(&l)?)?;
        }
        Ok(CycleOutcome {
            dir: dir.clone(),
            cycle_id: self.head.cycle_id.clone(),
            portfolio: week.portfolio.clone(),
            inputs_sha256,
            decision_sha256: decision,
            manifest_sha256,
            episodes: learned.episodes.len(),
            stages: stages.runs,
            week,
        })
    }

    /// One model stage; `goal` `Err` = skipped, with why.
    async fn stage(
        &self,
        s: &mut Stages,
        stage: Stage,
        agent: &str,
        goal: std::result::Result<String, &str>,
    ) {
        match (self.env.runner, goal) {
            (Some(r), Ok(goal)) => {
                let req = StageRequest {
                    stage,
                    agent: agent.to_string(),
                    dir: self.dir.clone(),
                    cycle_id: self.head.cycle_id.clone(),
                    goal,
                };
                let (run, line) = run_stage(r, self.env.clock, req).await;
                s.runs.push(run);
                s.lines.push(line);
            }
            (None, _) => s
                .lines
                .push(StageLine::skipped(stage, agent, "no stage runner")),
            (Some(_), Err(why)) => s.lines.push(StageLine::skipped(stage, agent, why)),
        }
    }

    /// Architect, then Critic, each inside its phase (module table).
    async fn model_stages(&self, s: &mut Stages) -> Result<()> {
        let (store, p, dir) = (self.env.store, self.p, &self.dir);
        open_phase(store, dir, Phase::Propose)?;
        let goal = architect_goal(&self.head, self.index.refs.len(), &self.carried);
        self.stage(s, Stage::Architect, &p.architect, Ok(goal))
            .await;
        open_phase(store, dir, Phase::Challenge)?;
        let candidates = submit::candidates(store, dir)?;
        let goal = if candidates.is_empty() {
            Err("no candidate to challenge")
        } else {
            Ok(critic_goal(&self.head, &candidates))
        };
        self.stage(s, Stage::Challenge, &p.critic, goal).await;
        open_phase(store, dir, Phase::Closed)
    }

    /// New + carried proposals and challenges (module table: Decide).
    fn assemble(&self) -> Result<WeekRecords> {
        let store = self.env.store;
        let fresh = store.proposals(&self.dir)?;
        let fresh_by_opp: BTreeMap<String, String> = fresh
            .iter()
            .map(|x| (x.opportunity().id.clone(), x.id.clone()))
            .collect();
        let mut proposals = fresh;
        let mut superseded = Vec::new();
        for c in &self.carried.proposals {
            let opp = &c.opportunity().id;
            match fresh_by_opp.get(opp) {
                Some(new) => superseded.push((c.id.clone(), new.clone(), opp.clone())),
                None => {
                    // Its forecast stays frozen in its own cycle.
                    let mut q = c.clone();
                    q.draft.forecast.clear();
                    proposals.push(q);
                }
            }
        }
        proposals.sort_by(|a, b| a.id.cmp(&b.id));
        let targets: Vec<&Opportunity> = proposals.iter().map(|x| x.opportunity()).collect();
        let mut dropped = self.carried.dropped.clone();
        let mut challenges = Vec::new();
        for c in &self.carried.challenges {
            match c.draft.check(&self.index, &targets) {
                Ok(()) => challenges.push(c.clone()),
                Err(e) => dropped.push(Dropped {
                    kind: "CHALLENGE".into(),
                    record: c.id.clone(),
                    candidate: c.draft.target.clone(),
                    why: e.iter().map(ToString::to_string).collect(),
                }),
            }
        }
        challenges.extend(store.challenges(&self.dir)?);
        challenges.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(WeekRecords {
            proposals,
            challenges,
            superseded,
            dropped,
        })
    }

    /// The common fields of a candidate event, then `fields`.
    fn event(&self, event: &str, fields: Value) -> Value {
        let mut e = json!({
            "schema": EVENT_SCHEMA,
            "event": event,
            "run": self.dir.to_string(),
            "cycle_id": self.head.cycle_id,
            "decided_at_ms": self.head.decided_at_ms,
        });
        if let (Some(m), Value::Object(f)) = (e.as_object_mut(), fields) {
            m.extend(f);
        }
        e
    }

    /// Per decided candidate (by id): its `decided.json` row, its event and
    /// its episode; then a `SUPERSEDED` / `DROPPED` event per carry change.
    fn learn(&self, head: &WeekHead, week: &Week, r: &WeekRecords) -> Result<Learned> {
        let by_id: BTreeMap<&str, &MechanismProposal> =
            r.proposals.iter().map(|x| (x.id.as_str(), x)).collect();
        let notes: BTreeMap<&str, &str> = week
            .notes
            .iter()
            .map(|n| (n.id.as_str(), n.why.as_str()))
            .collect();
        let mut sorted: Vec<&Decided> = week.decided.iter().collect();
        sorted.sort_by(|a, b| a.assessment.id.cmp(&b.assessment.id));
        let mut out = Learned {
            rows: Vec::new(),
            events: Vec::new(),
            episodes: Vec::new(),
            active_missing: self
                .p
                .active
                .keys()
                .filter(|id| !sorted.iter().any(|d| &d.assessment.id == *id))
                .cloned()
                .collect(),
        };
        for (i, d) in sorted.into_iter().enumerate() {
            let a = &d.assessment;
            let x = by_id[d.proposal.as_str()];
            let (list, rank, action) = placement(&week.portfolio, &a.id)?;
            let why = notes.get(a.id.as_str()).copied().unwrap_or("");
            let ep = episode(i + 1, d, head, why, action)?;
            let core = json!({
                "candidate": a.id,
                "opportunity_version": a.version,
                "proposal": x.id,
                "proposal_cycle": x.cycle_id,
                "carried": x.cycle_id != self.head.cycle_id,
                "list": list,
                "rank": rank,
                "action": action,
                "verdict": a.verdict.verdict,
                "gates": a.verdict.labels(),
                "inputs_sha256": a.inputs_sha256,
                "episode": ep.id,
            });
            let mut row = core.clone();
            if let Some(m) = row.as_object_mut() {
                m.insert("track".into(), serde_json::to_value(d.track)?);
                m.insert("note".into(), json!(why));
                m.insert("changes".into(), serde_json::to_value(&d.applied.changes)?);
                m.insert("ignored".into(), serde_json::to_value(&d.applied.ignored)?);
            }
            out.rows.push(row);
            out.events.push(self.event("DECIDED", core));
            out.episodes.push(ep);
        }
        for (old, new, opp) in &r.superseded {
            out.events.push(self.event(
                "SUPERSEDED",
                json!({"candidate": opp, "record": old, "by": new}),
            ));
        }
        for d in &r.dropped {
            out.events.push(self.event(
                "DROPPED",
                json!({"candidate": d.candidate, "record": d.record, "kind": d.kind, "why": d.why}),
            ));
        }
        Ok(out)
    }

    /// `decided.json`.
    fn decided(&self, r: &WeekRecords, l: &Learned) -> Value {
        json!({
            "schema": "soe.cycle_decided/1",
            "run": self.dir.to_string(),
            "cycle_id": self.head.cycle_id,
            "candidates": l.rows,
            "superseded": r
                .superseded
                .iter()
                .map(|(old, new, opp)| json!({"record": old, "by": new, "candidate": opp}))
                .collect::<Vec<_>>(),
            "dropped": r.dropped,
            "active_missing": l.active_missing,
        })
    }

    /// This cycle's own predictions, frozen at the decision with the
    /// portfolio (a carried one is frozen in its own cycle already).
    fn forecast(&self, week: &Week, r: &WeekRecords) -> Result<Forecast> {
        let own: Vec<(&str, u32, &[ForecastItem])> = r
            .proposals
            .iter()
            .filter(|x| x.cycle_id == self.head.cycle_id)
            .map(|x| {
                let o = x.opportunity();
                (o.id.as_str(), o.version, x.draft.forecast.as_slice())
            })
            .collect();
        let f = freeze_forecast(
            &self.head.cycle_id,
            self.head.decided_at(),
            &week.portfolio,
            &own,
        )
        .map_err(|e| anyhow!("forecast: {}", joined(&e)))?;
        let horizon = horizon_problems(&f, self.p.forecast_max_weeks);
        if !horizon.is_empty() {
            bail!("forecast: {}", joined(&horizon));
        }
        Ok(f)
    }

    /// `inputs.json` and its `inputs_sha256` (module table: Report).
    fn inputs(&self, week: &Week, r: &WeekRecords) -> Result<(Value, String)> {
        let (p, profile) = (self.p, self.env.profile.record);
        let mut proposals = Vec::new();
        for x in &r.proposals {
            let o = x.opportunity();
            // The stored record (a carried one with its forecast).
            let stored = self
                .carried
                .proposals
                .iter()
                .find(|c| c.id == x.id)
                .unwrap_or(x);
            proposals.push(json!({
                "id": x.id,
                "cycle_id": x.cycle_id,
                "opportunity": o.id,
                "version": o.version,
                "sha256": record_sha256(stored)?,
            }));
        }
        let mut challenges = Vec::new();
        for c in &r.challenges {
            challenges.push(json!({
                "id": c.id,
                "cycle_id": c.cycle_id,
                "target": c.draft.target,
                "sha256": record_sha256(c)?,
            }));
        }
        let mut inputs = json!({
            "schema": "soe.cycle_inputs/1",
            "run": self.dir.to_string(),
            "cycle_id": self.head.cycle_id,
            "week": p.week,
            "decided_at": self.head.decided_at(),
            "decided_at_ms": self.head.decided_at_ms,
            "mode": self.head.mode,
            "packet_sha256": self.head.packet_sha256,
            "packet_records": self.index.refs.len(),
            "profile": {
                "id": profile.id,
                "version": profile.version,
                "sha256": self.env.profile.sha256,
            },
            "policy_sha256": policy_sha256(&profile.rank_order),
            "generation": p.generation,
            "economics_version": ECONOMICS_VERSION,
            "proposals": proposals,
            "challenges": challenges,
            "active": p.active,
            "budget": Budget::of(profile),
            "portfolio_inputs_sha256": week.portfolio.inputs_sha256,
        });
        let sha = canonical_sha256(&inputs);
        inputs["inputs_sha256"] = json!(sha);
        Ok((inputs, sha))
    }
}
