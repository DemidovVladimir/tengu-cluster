//! Historical replay (roadmap O4 "evaluation set plus at least 8 older dated
//! cases unseen during prompt / policy drafting"; § 13 generation
//! isolation): every case of a `soe.replay_set/1` (`domain/soe/replay.rs`)
//! re-run as a cycle on its decision date under `replays/` — never
//! `cycles/`, never a live state log — then scored against the operator's
//! labels. A holdout label, outcome or score is shown only after a counted
//! read (the xlab rule, `tools/xlab/holdout.rs`).
//!
//! | Step | Writes | Rule |
//! |---|---|---|
//! | Refuse | — | the set's profile is not the profile in force (`profile_mismatch`); an invalid run id or a case dir id over 80 chars (`invalid_id`); `holdout` on a set without a `HOLDOUT` case (`holdout_empty`); the run id used before (`replay_run_exists`) — before anything is created |
//! | Claim | `replays/<run id>/` | the report dir first: a run id is used once |
//! | Cases | `replays/<run id>.<case id>/` per case | `run_cycle` (`Target::Replay`: the `knowable` clock, no carry, no state log); `DEVELOPMENT` cases always, `HOLDOUT` cases only with `holdout` — otherwise not run, only counted |
//! | Stages | (the case dir) | the case's recorded drafts when it has any ([`RecordedStages`]: through `submit_*` like a tool, stamped `engine = model = "recorded"`, the set's sha256 as `skill_sha256`; a refused draft fails the stage, fail-soft); else the caller's runner — none (`--no-llm`) proposes nothing: a `HOLD` week |
//! | Score | — | `domain::soe::replay`: `score_case` per case, `summarize` per split, `rank_stability` of each decided week at `scale_bps`, the unsupported claims of its proposals |
//! | Count | `holdout-reads.jsonl` | with `holdout`: one `soe.holdout_read/1` line right after the claim, before any case runs (a frozen holdout case dir is readable at once, and a run that stops part-way still counted its read); unrecordable ⇒ nothing runs (the report dir stays open) |
//! | Report | `report.json` (`soe.replay_report/1`, canonical), `report.md`, `MANIFEST.json` (frozen); one `replays.jsonl` line (`soe.replay_run/1`) | case rows: answer, portfolio and decision sha256, stability, unsupported claims; label, outcome note and score only for development cases and counted holdout cases |
//!
//! | Holdout read line | Value |
//! |---|---|
//! | fields | `schema`, `via = "replay"`, `run`, `set`, `set_version`, `set_sha256`, `cases` (the holdout ids read), `generation`, `policy_sha256`, `profile_sha256`, `read_at_ms` — ids and hashes in full |
//! | `#n` | the line's place among the reads of its `set_sha256` up to its own line (a concurrent read never takes its number); the line number is the ledger's |
//! | `read_at_ms` | the clock when the line is written: before the holdout cases run |
//!
//! Same set, profile, generation, stage records and run id ⇒ the same case
//! dirs (`decision_sha256`) and the same `report.json` bytes.

use std::fmt::Write as _;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use super::cycle::{policy_sha256, run_cycle, CycleEnv, CycleOutcome, CycleParams, Target};
use super::freeze::freeze;
use super::submit::{json_line, submit_challenge, submit_proposal, GenerationPin};
use crate::domain::canonical::canonical_json;
use crate::domain::lineage::value::{valid_id, Time};
use crate::domain::soe::forecast::sha256_of;
use crate::domain::soe::ops::{Stage, TokenPrices};
use crate::domain::soe::portfolio::IsoWeek;
use crate::domain::soe::proposal::Provenance;
use crate::domain::soe::replay::{
    rank_stability, score_case, summarize, CaseScore, Label, ReplayCase, ReplaySet, Scored, Split,
    Stability, Summary, WeekAnswer,
};
use crate::domain::soe::value::codes;
use crate::ports::soe::{
    CycleStore, RunDir, RunStatus, StageReply, StageRequest, StageRunner, StateLog,
};

pub(crate) const REPORT_JSON: &str = "report.json";
pub(crate) const REPORT_MD: &str = "report.md";
pub(crate) const REPORT_SCHEMA: &str = "soe.replay_report/1";
pub(crate) const RUN_SCHEMA: &str = "soe.replay_run/1";
pub(crate) const READ_SCHEMA: &str = "soe.holdout_read/1";
/// `holdout` asked of a set without a holdout case.
pub(crate) const HOLDOUT_EMPTY: &str = "holdout_empty";
pub(crate) const REPLAY_RUN_EXISTS: &str = "replay_run_exists";
/// `engine` / `model` of a recorded draft's stamp.
pub(crate) const RECORDED: &str = "recorded";

/// The set as loaded: the record and its file's digest.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SetIn<'a> {
    pub record: &'a ReplaySet,
    /// `toml_digest` of the file (`config::soe::Loaded::sha256`).
    pub sha256: &'a str,
}

/// One replay's parameters (the caller takes the stage ones from `[soe]`).
#[derive(Debug, Clone)]
pub(crate) struct ReplayParams {
    /// The report dir `replays/<run id>`; case dirs `replays/<run id>.<case id>`.
    pub run_id: String,
    pub generation: GenerationPin,
    pub architect: String,
    pub critic: String,
    pub max_proposals: usize,
    pub forecast_max_weeks: u32,
    pub token_prices: Option<TokenPrices>,
    /// Run the `HOLDOUT` cases too and count the read.
    pub holdout: bool,
    /// The tornado scale of `rank_stability`.
    pub scale_bps: i32,
}

/// Where a case's stage records came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum StageSource {
    /// The set's recorded drafts.
    Recorded,
    /// The caller's runner (agents, or a cache of their runs).
    Runner,
    /// No runner, no drafts: nothing proposed.
    None,
}

/// One replayed case (module table: Report).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CaseRow {
    pub id: String,
    pub split: Split,
    pub week: IsoWeek,
    pub decided_at: Time,
    pub run: String,
    pub stages: StageSource,
    /// Recorded drafts the tools refused (fail-soft).
    pub refused_drafts: usize,
    pub answer: WeekAnswer,
    pub held: Vec<String>,
    pub rejected: Vec<String>,
    pub portfolio_sha256: String,
    pub decision_sha256: String,
    pub unsupported_claims: usize,
    pub stability: Stability,
    /// Development, or a counted holdout read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<CaseScore>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_note: Option<String>,
}

/// The holdout part of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HoldoutPart {
    pub cases: usize,
    pub read: bool,
    /// The ledger line (`holdout-reads.jsonl`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// Reads of this set's sha256 up to that line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set_reads: Option<usize>,
}

/// `report.json` (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReplayReport {
    pub schema: &'static str,
    pub run: String,
    pub set: String,
    pub set_version: u32,
    pub set_sha256: String,
    pub synthetic: bool,
    pub profile: String,
    pub profile_sha256: String,
    pub policy_sha256: String,
    pub generation: GenerationPin,
    pub scale_bps: i32,
    pub holdout: HoldoutPart,
    pub cases: Vec<CaseRow>,
    pub development: Summary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holdout_summary: Option<Summary>,
}

/// What [`run_replay`] answers.
#[derive(Debug, Clone)]
pub(crate) struct ReplayOutcome {
    pub dir: RunDir,
    pub report: ReplayReport,
    pub markdown: String,
    pub manifest_sha256: String,
    /// The `replays.jsonl` line number.
    pub line: u64,
}

/// A stage runner over a case's recorded drafts (module table: Stages).
pub(crate) struct RecordedStages<'a> {
    pub store: &'a dyn CycleStore,
    pub case: String,
    pub proposals: Vec<String>,
    pub challenges: Vec<String>,
    /// The run's generation id (a stamp must carry it).
    pub generation: String,
    /// The set's sha256: the "skill" the drafts came from.
    pub skill_sha256: String,
    pub decided_at_ms: i64,
    pub refused: Mutex<usize>,
}

#[async_trait]
impl StageRunner for RecordedStages<'_> {
    async fn run(&self, req: &StageRequest) -> Result<StageReply> {
        let drafts = match req.stage {
            Stage::Architect => &self.proposals,
            _ => &self.challenges,
        };
        let mut refusals = Vec::new();
        for (i, d) in drafts.iter().enumerate() {
            let stamp = Provenance {
                agent: req.agent.clone(),
                model: RECORDED.into(),
                engine: RECORDED.into(),
                skill_sha256: self.skill_sha256.clone(),
                generation: self.generation.clone(),
                call_id: format!("replay:{}:{:?}:{}", self.case, req.stage, i + 1),
                proposed_at: Time::At(self.decided_at_ms),
            };
            let refused = match req.stage {
                Stage::Architect => submit_proposal(self.store, &req.dir, d, stamp)?.err(),
                _ => submit_challenge(self.store, &req.dir, d, stamp)?.err(),
            };
            if let Some(e) = refused {
                refusals.push(format!(
                    "draft {}: {}",
                    i + 1,
                    e.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
            }
        }
        *self.refused.lock().unwrap_or_else(|p| p.into_inner()) += refusals.len();
        let ok = refusals.is_empty();
        Ok(StageReply {
            ok,
            summary: format!(
                "{} recorded draft(s), {} refused",
                drafts.len(),
                refusals.len()
            ),
            error: (!ok).then(|| refusals.join(" | ")),
            latency_ms: 0,
            metrics: Vec::new(),
        })
    }
}

fn draft_texts(items: &[Value]) -> Vec<String> {
    items.iter().map(canonical_json).collect()
}

/// The case dir id (module table).
fn case_dir(run_id: &str, case: &str) -> RunDir {
    RunDir::Replay(format!("{run_id}.{case}"))
}

/// The refusals before anything is created (module table: Refuse).
fn preflight(env: &CycleEnv, set: SetIn, p: &ReplayParams, run: &[&ReplayCase]) -> Result<()> {
    if let Err(e) = set.record.check_profile(env.profile.record) {
        bail!("{e}");
    }
    let mut bad = Vec::new();
    if !valid_id(&p.run_id) {
        bad.push(format!("run id `{}` is not an id", p.run_id));
    }
    for c in run {
        let d = case_dir(&p.run_id, &c.id);
        if !valid_id(d.id()) {
            bad.push(format!(
                "case dir `{}` is not an id (at most 80 chars)",
                d.id()
            ));
        }
    }
    if !bad.is_empty() {
        bail!("{}: {}", codes::INVALID_ID, bad.join("; "));
    }
    if p.holdout && set.record.cases_in(Split::Holdout).next().is_none() {
        bail!(
            "{HOLDOUT_EMPTY}: set `{}` has no HOLDOUT case — nothing to read",
            set.record.id
        );
    }
    let report = RunDir::Replay(p.run_id.clone());
    for d in std::iter::once(report).chain(run.iter().map(|c| case_dir(&p.run_id, &c.id))) {
        if env.store.status(&d)? != RunStatus::Absent {
            bail!(
                "{REPLAY_RUN_EXISTS}: {d} exists in {} — a replay run id is used once",
                env.store.root_display()
            );
        }
    }
    Ok(())
}

/// One case run to its frozen end.
async fn run_case(
    env: &CycleEnv<'_>,
    set: SetIn<'_>,
    p: &ReplayParams,
    c: &ReplayCase,
) -> Result<(CycleOutcome, StageSource, usize)> {
    let week = c
        .week()
        .with_context(|| format!("case `{}`: no ISO week", c.id))?;
    let at = c
        .decided_at_ms()
        .with_context(|| format!("case `{}`: decided_at is not an instant", c.id))?;
    let dir = case_dir(&p.run_id, &c.id);
    let params = CycleParams {
        target: Target::Replay {
            run_id: dir.id().to_string(),
        },
        week,
        decided_at_ms: at,
        generation: p.generation.clone(),
        architect: p.architect.clone(),
        critic: p.critic.clone(),
        max_proposals: p.max_proposals,
        forecast_max_weeks: p.forecast_max_weeks,
        active: Default::default(),
        token_prices: p.token_prices,
    };
    if c.has_drafts() {
        let recorded = RecordedStages {
            store: env.store,
            case: c.id.clone(),
            proposals: draft_texts(&c.proposals),
            challenges: draft_texts(&c.challenges),
            generation: p.generation.id.clone(),
            skill_sha256: set.sha256.to_string(),
            decided_at_ms: at,
            refused: Mutex::new(0),
        };
        let mut e = *env;
        e.runner = Some(&recorded);
        let out = run_cycle(&e, &params)
            .await
            .with_context(|| format!("case `{}`", c.id))?;
        let n = *recorded.refused.lock().unwrap_or_else(|p| p.into_inner());
        return Ok((out, StageSource::Recorded, n));
    }
    let source = if env.runner.is_some() {
        StageSource::Runner
    } else {
        StageSource::None
    };
    let out = run_cycle(env, &params)
        .await
        .with_context(|| format!("case `{}`", c.id))?;
    Ok((out, source, 0))
}

/// The case's row; its score when `scored`.
fn row(
    env: &CycleEnv,
    p: &ReplayParams,
    c: &ReplayCase,
    run: (CycleOutcome, StageSource, usize),
    scored: bool,
) -> Result<CaseRow> {
    let (out, stages, refused_drafts) = run;
    let answer = WeekAnswer::of(&out.portfolio);
    let unsupported_claims = env
        .store
        .proposals(&out.dir)?
        .iter()
        .map(|x| x.draft.unsupported().len())
        .sum();
    let stability = rank_stability(&out.week, env.profile.record, p.scale_bps)
        .map_err(|e| anyhow::anyhow!("case `{}`: rank stability: {e}", c.id))?;
    Ok(CaseRow {
        id: c.id.clone(),
        split: c.split,
        week: out.portfolio.week,
        decided_at: c.decided_at,
        run: out.dir.to_string(),
        stages,
        refused_drafts,
        held: out.portfolio.held.iter().map(|r| r.id.clone()).collect(),
        rejected: out
            .portfolio
            .rejected
            .iter()
            .map(|r| r.id.clone())
            .collect(),
        portfolio_sha256: sha256_of(&out.portfolio).map_err(|e| anyhow::anyhow!("{e}"))?,
        decision_sha256: out.decision_sha256.clone(),
        unsupported_claims,
        stability,
        score: scored.then(|| score_case(c, &answer)),
        outcome_note: scored.then(|| c.outcome_note.clone()),
        answer,
    })
}

fn summary_of(rows: &[&CaseRow]) -> Summary {
    let scored: Vec<Scored> = rows
        .iter()
        .filter_map(|r| {
            r.score.as_ref().map(|s| Scored {
                answer: &r.answer,
                score: s,
                unsupported_claims: r.unsupported_claims,
                stability: &r.stability,
            })
        })
        .collect();
    summarize(&scored)
}

/// Module table: the holdout read, counted (line, reads of this set).
fn count_read(
    env: &CycleEnv,
    set: SetIn,
    p: &ReplayParams,
    cases: &[String],
) -> Result<(u64, usize)> {
    let line = json!({
        "schema": READ_SCHEMA,
        "via": "replay",
        "run": RunDir::Replay(p.run_id.clone()).to_string(),
        "set": set.record.id,
        "set_version": set.record.version,
        "set_sha256": set.sha256,
        "cases": cases,
        "generation": p.generation,
        "policy_sha256": policy_sha256(&env.profile.record.rank_order),
        "profile_sha256": env.profile.sha256,
        "read_at_ms": env.clock.now_ms(),
    });
    let n = env
        .store
        .append_line(StateLog::HoldoutReads, &canonical_json(&line))
        .context("holdout read not recorded — nothing is shown")?;
    let upto = usize::try_from(n).unwrap_or(usize::MAX);
    let reads = env
        .store
        .lines(StateLog::HoldoutReads)?
        .iter()
        .take(upto)
        .filter(|l| {
            serde_json::from_str::<Value>(l)
                .ok()
                .is_some_and(|v| v["set_sha256"] == json!(set.sha256))
        })
        .count()
        .max(1);
    Ok((n, reads))
}

/// Module table: replay `set` (the runs, the count, the report).
pub(crate) async fn run_replay(
    env: &CycleEnv<'_>,
    set: SetIn<'_>,
    p: &ReplayParams,
) -> Result<ReplayOutcome> {
    let run: Vec<&ReplayCase> = set
        .record
        .cases
        .iter()
        .filter(|c| c.split == Split::Development || p.holdout)
        .collect();
    preflight(env, set, p, &run)?;
    let dir = RunDir::Replay(p.run_id.clone());
    env.store
        .claim(&dir)
        .with_context(|| format!("claim {dir} in {}", env.store.root_display()))?;

    // Counted before any holdout case runs: a case dir is readable
    // (`tengu soe show`) the moment it freezes, and a run that stops part-way
    // writes no report — a read is never left uncounted (overcounting is the
    // safe side).
    let holdout_ids: Vec<String> = run
        .iter()
        .filter(|c| c.split == Split::Holdout)
        .map(|c| c.id.clone())
        .collect();
    let read = if p.holdout {
        Some(count_read(env, set, p, &holdout_ids)?)
    } else {
        None
    };
    let mut rows = Vec::new();
    for c in &run {
        let out = run_case(env, set, p, c).await?;
        rows.push(row(env, p, c, out, true)?);
    }
    let dev: Vec<&CaseRow> = rows
        .iter()
        .filter(|r| r.split == Split::Development)
        .collect();
    let held_out: Vec<&CaseRow> = rows.iter().filter(|r| r.split == Split::Holdout).collect();
    let report = ReplayReport {
        schema: REPORT_SCHEMA,
        run: dir.to_string(),
        set: set.record.id.clone(),
        set_version: set.record.version,
        set_sha256: set.sha256.to_string(),
        synthetic: set.record.synthetic,
        profile: env.profile.record.id.clone(),
        profile_sha256: env.profile.sha256.to_string(),
        policy_sha256: policy_sha256(&env.profile.record.rank_order),
        generation: p.generation.clone(),
        scale_bps: p.scale_bps,
        holdout: HoldoutPart {
            cases: set.record.cases_in(Split::Holdout).count(),
            read: read.is_some(),
            line: read.map(|r| r.0),
            set_reads: read.map(|r| r.1),
        },
        development: summary_of(&dev),
        holdout_summary: read.map(|_| summary_of(&held_out)),
        cases: rows.clone(),
    };
    let markdown = render(&report);
    env.store.write(&dir, REPORT_JSON, &json_line(&report)?)?;
    env.store.write(&dir, REPORT_MD, markdown.as_bytes())?;
    let manifest_sha256 = freeze(env.store, &dir)?;
    let line = env.store.append_line(
        StateLog::Replays,
        &canonical_json(&json!({
            "schema": RUN_SCHEMA,
            "run": dir.to_string(),
            "set": set.record.id,
            "set_sha256": set.sha256,
            "generation": p.generation,
            "policy_sha256": report.policy_sha256,
            "cases": rows
                .iter()
                .map(|r| json!({"case": r.id, "split": r.split, "run": r.run}))
                .collect::<Vec<_>>(),
            "holdout_hidden": report.holdout.cases - held_out.len(),
            "holdout_read_line": report.holdout.line,
            "development": report.development,
            "holdout": report.holdout_summary,
            "manifest_sha256": manifest_sha256,
        })),
    )?;
    Ok(ReplayOutcome {
        dir,
        report,
        markdown,
        manifest_sha256,
        line,
    })
}

// ---------------------------------------------------------------------------
// report.md
// ---------------------------------------------------------------------------

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn bps(v: Option<crate::domain::soe::value::Bps>) -> String {
    v.map_or("-".to_string(), |b| format!("{b} bps"))
}

fn label_name(l: Label) -> String {
    serde_json::to_value(l)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn summary_rows(out: &mut String, name: &str, s: &Summary) {
    let _ = writeln!(
        out,
        "| {name} | {} | {} ({}) | {} / {} | {} | {} | {} · {} | {} | {} of {} ({}) |",
        s.cases,
        s.agree,
        bps(s.agreement_bps),
        s.held_right,
        s.held,
        bps(s.hold_precision_bps),
        bps(s.hold_recall_bps),
        s.missed,
        s.false_positives,
        s.unsupported_claims,
        s.stable,
        s.perturbations,
        bps(s.stability_bps)
    );
}

/// `report.md`: the summary per split, every case, every moved ranking.
pub(crate) fn render(r: &ReplayReport) -> String {
    let mut out = format!(
        "# SOE replay `{}`\n\n| Field | Value |\n|---|---|\n| Set | `{}` v{} · sha256 `{}`{} |\n| Profile | `{}` · sha256 `{}` |\n| Policy sha256 | `{}` |\n| Generation | `{}` · sha256 `{}` |\n| Rank stability scale | ± {} bps |\n",
        r.run,
        r.set,
        r.set_version,
        r.set_sha256,
        if r.synthetic { " · SYNTHETIC" } else { "" },
        r.profile,
        r.profile_sha256,
        r.policy_sha256,
        r.generation.id,
        r.generation.sha256,
        r.scale_bps
    );
    let holdout = match (r.holdout.read, r.holdout.line, r.holdout.set_reads) {
        (true, Some(line), Some(k)) => format!(
            "{} case(s) READ — holdout read #{k} of this set (holdout-reads.jsonl line {line}); a policy or prompt changed after this read is fitted to it: say so",
            r.holdout.cases
        ),
        _ => format!(
            "{} case(s) hidden — not run; labels and outcomes stay unread until a counted read (`--holdout`)",
            r.holdout.cases
        ),
    };
    let _ = writeln!(out, "| Holdout | {holdout} |\n");
    out.push_str("## Summary\n\n| Split | Cases | Agree | HOLD right / held | HOLD precision | HOLD recall | Missed · false + | Unsupported claims | Stable rankings |\n|---|---|---|---|---|---|---|---|---|\n");
    summary_rows(&mut out, "DEVELOPMENT", &r.development);
    if let Some(h) = &r.holdout_summary {
        summary_rows(&mut out, "HOLDOUT", h);
    }
    out.push_str("\n## Cases\n\n| Case | Split | Week | Stages | Answer | Label | Agrees | Missed | False + | Unsupported | Stable | Run |\n|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for c in &r.cases {
        let answer = if c.answer.hold {
            "HOLD".to_string()
        } else {
            format!("ranked {}", c.answer.ranked.join(", "))
        };
        let (label, agrees, missed, fp) = match &c.score {
            Some(s) => (
                label_name(s.label),
                if s.agrees { "yes" } else { "NO" }.to_string(),
                s.missed.join(", "),
                s.false_positives.join(", "),
            ),
            None => ("(hidden)".into(), "-".into(), "-".into(), "-".into()),
        };
        let stages = match c.stages {
            StageSource::Recorded if c.refused_drafts > 0 => {
                format!("RECORDED ({} refused)", c.refused_drafts)
            }
            StageSource::Recorded => "RECORDED".into(),
            StageSource::Runner => "RUNNER".into(),
            StageSource::None => "NONE".into(),
        };
        let _ = writeln!(
            out,
            "| `{}` | {:?} | {} | {stages} | {} | {label} | {agrees} | {} | {} | {} | {} of {} | `{}` |",
            c.id,
            c.split,
            c.week,
            cell(&answer),
            cell(&missed),
            cell(&fp),
            c.unsupported_claims,
            c.stability.stable,
            c.stability.perturbations,
            c.run
        );
    }
    let notes: Vec<&CaseRow> = r
        .cases
        .iter()
        .filter(|c| c.outcome_note.is_some())
        .collect();
    if !notes.is_empty() {
        out.push_str("\n## Outcomes (the operator's notes)\n\n");
        for c in notes {
            let _ = writeln!(
                out,
                "- `{}`: {}",
                c.id,
                cell(c.outcome_note.as_deref().unwrap_or_default())
            );
        }
    }
    let moved: Vec<(&CaseRow, &crate::domain::soe::replay::Moved)> = r
        .cases
        .iter()
        .flat_map(|c| c.stability.moved.iter().map(move |m| (c, m)))
        .collect();
    if !moved.is_empty() {
        out.push_str("\n## Rankings that moved\n\n| Case | Perturbation | Before | After |\n|---|---|---|---|\n");
        for (c, m) in moved {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} |",
                c.id,
                m.perturbation,
                m.before.join(", "),
                m.after.join(", ")
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::application::soe::cycle::ProfileIn;
    use crate::application::soe::tests::{
        build_records, chain_specs, drafts, generation, load_case, Bench,
    };
    use crate::domain::source::SourceRecord;

    const SET_SHA: &str = "00000000000000000000000000000000000000000000000000000000000000aa";

    /// A replay set of cycle cases: their records stored, their drafts
    /// recorded; `label` and `split` per case id.
    fn set_of(bench: &Bench, cases: &[(&str, Split, Label)]) -> ReplaySet {
        let mut rows = Vec::new();
        for (id, split, label) in cases {
            let case = load_case(id);
            let (by, _) = build_records(&chain_specs(&case));
            let stored: Vec<SourceRecord> = case
                .records
                .iter()
                .filter(|r| r.stored)
                .map(|r| by[&r.native].clone())
                .collect();
            bench.sources.add(&stored);
            let as_values = |items: &[toml::Value]| -> Vec<Value> {
                drafts(items, &by)
                    .iter()
                    .map(|t| serde_json::from_str(t).unwrap())
                    .collect()
            };
            rows.push(ReplayCase {
                id: id.replace('_', "-"),
                decided_at: case.decided_at,
                week: Some(case.week.parse().unwrap()),
                split: *split,
                label: *label,
                good: Vec::new(),
                bad: Vec::new(),
                outcome_note: format!("synthetic outcome of {id}"),
                proposals: as_values(&case.proposals),
                challenges: as_values(&case.challenges),
            });
        }
        ReplaySet {
            schema: "soe.replay_set/1".parse().unwrap(),
            id: "synthetic-replay".into(),
            version: 1,
            synthetic: true,
            profile: "synthetic-operator".into(),
            note: "cycle cases replayed".into(),
            cases: rows,
        }
    }

    fn params(run_id: &str, holdout: bool) -> ReplayParams {
        ReplayParams {
            run_id: run_id.into(),
            generation: generation(),
            architect: "soe-architect".into(),
            critic: "soe-critic".into(),
            max_proposals: 12,
            forecast_max_weeks: 12,
            token_prices: None,
            holdout,
            scale_bps: 2000,
        }
    }

    async fn replay(bench: &Bench, set: &ReplaySet, p: &ReplayParams) -> Result<ReplayOutcome> {
        let env = CycleEnv {
            sources: Some(&bench.sources),
            registry: &bench.registry,
            store: &*bench.store,
            runner: None,
            clock: &bench.clock,
            profile: ProfileIn {
                record: &bench.profile.record,
                sha256: &bench.profile.sha256,
                text: &bench.text,
            },
        };
        run_replay(
            &env,
            SetIn {
                record: set,
                sha256: SET_SHA,
            },
            p,
        )
        .await
    }

    const CASES: [(&str, Split, Label); 3] = [
        ("strong_news_weak_demand", Split::Development, Label::Hold),
        ("no_candidate_passes", Split::Development, Label::Good),
        ("high_ticket_integration", Split::Holdout, Label::Bad),
    ];

    fn text(o: &ReplayOutcome) -> String {
        format!(
            "{}\n{}",
            canonical_json(&serde_json::to_value(&o.report).unwrap()),
            o.markdown
        )
    }

    /// Without `holdout` a holdout case is never run and nothing of its
    /// label or outcome is shown or written; with it, one read is counted
    /// before the report, and the label shows with its read number.
    #[tokio::test]
    async fn holdout_label_hidden_until_counted_read() {
        let bench = Bench::new();
        let set = set_of(&bench, &CASES);
        let hidden = replay(&bench, &set, &params("r-1", false)).await.unwrap();
        let ids: Vec<&str> = hidden.report.cases.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["strong-news-weak-demand", "no-candidate-passes"]);
        assert_eq!(
            hidden.report.holdout,
            HoldoutPart {
                cases: 1,
                read: false,
                line: None,
                set_reads: None
            }
        );
        assert!(hidden.report.holdout_summary.is_none());
        let shown = text(&hidden);
        assert!(!shown.contains("high-ticket-integration"), "{shown}");
        assert!(
            !shown.contains("synthetic outcome of high_ticket"),
            "{shown}"
        );
        assert!(shown.contains("1 case(s) hidden"), "{shown}");
        let m = bench.store.snapshot();
        assert!(
            m.dirs.keys().all(|d| !d.id().contains("high-ticket")),
            "a hidden case is never run"
        );
        assert!(bench
            .store
            .lines(StateLog::HoldoutReads)
            .unwrap()
            .is_empty());
        // Development labels show; their scores count.
        assert_eq!(hidden.report.development.cases, 2);
        let dev = &hidden.report.cases[0];
        assert!(dev.answer.hold && dev.score.as_ref().unwrap().agrees);
        assert!(!hidden.report.cases[1].score.as_ref().unwrap().agrees);

        // A counted read: the line comes first, then the label shows.
        let read = replay(&bench, &set, &params("r-2", true)).await.unwrap();
        let lines = bench.store.lines(StateLog::HoldoutReads).unwrap();
        assert_eq!(lines.len(), 1);
        let l: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(l["schema"], READ_SCHEMA);
        assert_eq!(l["cases"], json!(["high-ticket-integration"]));
        assert_eq!(l["set_sha256"], SET_SHA);
        assert_eq!(l["run"], "replays/r-2");
        assert_eq!(l["generation"]["id"], "SOE-G0");
        assert_eq!(read.report.holdout.line, Some(1));
        assert_eq!(read.report.holdout.set_reads, Some(1));
        let h = read
            .report
            .cases
            .iter()
            .find(|c| c.split == Split::Holdout)
            .unwrap();
        assert_eq!(h.score.as_ref().unwrap().label, Label::Bad);
        assert!(text(&read).contains("holdout read #1 of this set"));
        assert_eq!(read.report.holdout_summary.as_ref().unwrap().cases, 1);
        // A second read is #2; a set without a holdout case refuses `holdout`.
        let again = replay(&bench, &set, &params("r-3", true)).await.unwrap();
        assert_eq!(again.report.holdout.set_reads, Some(2));
        let mut dev_only = set.clone();
        dev_only.cases.retain(|c| c.split == Split::Development);
        let e = replay(&bench, &dev_only, &params("r-4", true))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").starts_with(HOLDOUT_EMPTY), "{e:#}");
        assert_eq!(
            bench.store.status(&RunDir::Replay("r-4".into())).unwrap(),
            RunStatus::Absent
        );
    }

    /// Answers every stage with nothing; notes how many holdout reads the
    /// ledger held when each case's stage ran.
    struct LedgerProbe {
        store: std::sync::Arc<crate::application::soe::tests::MemCycleStore>,
        seen: Mutex<Vec<(String, usize)>>,
    }

    #[async_trait]
    impl StageRunner for LedgerProbe {
        async fn run(&self, req: &StageRequest) -> Result<StageReply> {
            let reads = self.store.lines(StateLog::HoldoutReads)?.len();
            self.seen
                .lock()
                .unwrap()
                .push((req.dir.id().to_string(), reads));
            Ok(StageReply {
                ok: true,
                ..StageReply::default()
            })
        }
    }

    /// The read is counted before a holdout case runs — a frozen case dir
    /// is readable at once, so a run that stops after it never leaves an
    /// uncounted read.
    #[tokio::test]
    async fn holdout_read_counted_before_a_holdout_case_runs() {
        let bench = Bench::new();
        let mut set = set_of(&bench, &CASES);
        for c in &mut set.cases {
            c.proposals.clear();
            c.challenges.clear();
        }
        let probe = LedgerProbe {
            store: std::sync::Arc::clone(&bench.store),
            seen: Mutex::new(Vec::new()),
        };
        let env = CycleEnv {
            sources: Some(&bench.sources),
            registry: &bench.registry,
            store: &*bench.store,
            runner: Some(&probe),
            clock: &bench.clock,
            profile: ProfileIn {
                record: &bench.profile.record,
                sha256: &bench.profile.sha256,
                text: &bench.text,
            },
        };
        let set_in = SetIn {
            record: &set,
            sha256: SET_SHA,
        };
        run_replay(&env, set_in, &params("r-p", true))
            .await
            .unwrap();
        let seen = probe.seen.lock().unwrap().clone();
        let holdout: Vec<&(String, usize)> = seen
            .iter()
            .filter(|(d, _)| d.contains("high-ticket"))
            .collect();
        assert_eq!(
            holdout,
            [&("r-p.high-ticket-integration".to_string(), 1)],
            "{seen:?}"
        );
        assert!(seen.iter().all(|(_, n)| *n == 1), "{seen:?}");
        // Without `holdout` nothing is counted, whatever runs.
        let probe_dev = LedgerProbe {
            store: std::sync::Arc::clone(&bench.store),
            seen: Mutex::new(Vec::new()),
        };
        let env = CycleEnv {
            runner: Some(&probe_dev),
            ..env
        };
        run_replay(&env, set_in, &params("r-q", false))
            .await
            .unwrap();
        assert_eq!(bench.store.lines(StateLog::HoldoutReads).unwrap().len(), 1);
        assert!(probe_dev
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|(d, _)| !d.contains("high-ticket")));
    }

    /// A replay writes `replays/` only: no `cycles/` dir, no live state log
    /// but `replays.jsonl` (and the holdout ledger on a read); a run id is
    /// used once.
    #[tokio::test]
    async fn replay_never_writes_cycles_dir() {
        let bench = Bench::new();
        let set = set_of(&bench, &CASES);
        let out = replay(&bench, &set, &params("r-1", true)).await.unwrap();
        let m = bench.store.snapshot();
        assert!(m.dirs.keys().all(|d| d.is_replay()), "{:?}", m.dirs.keys());
        assert_eq!(bench.store.cycles().unwrap(), Vec::<String>::new());
        let logs: Vec<StateLog> = m.logs.keys().copied().collect();
        assert_eq!(logs, [StateLog::HoldoutReads, StateLog::Replays]);
        assert_eq!(out.line, 1);
        // Every dir frozen: the report and the three cases.
        assert_eq!(m.frozen.len(), 4);
        let files: Vec<String> = bench.store.files(&out.dir).unwrap();
        assert_eq!(files, [REPORT_JSON, REPORT_MD]);
        let e = replay(&bench, &set, &params("r-1", false))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").starts_with(REPLAY_RUN_EXISTS), "{e:#}");
        // Another profile's labels are refused.
        let mut other = set.clone();
        other.profile = "someone-else".into();
        let e = replay(&bench, &other, &params("r-9", false))
            .await
            .unwrap_err();
        assert!(
            format!("{e:#}").starts_with(codes::PROFILE_MISMATCH),
            "{e:#}"
        );
    }

    /// Recorded drafts replay offline: the same set and run id in a fresh
    /// state give the same case dirs and the same report bytes; another run
    /// id decides the same portfolios.
    #[tokio::test]
    async fn offline_replay_identical() {
        let run = |run_id: &'static str| async move {
            let bench = Bench::new();
            let set = set_of(&bench, &CASES);
            let out = replay(&bench, &set, &params(run_id, true)).await.unwrap();
            let snap = bench.store.snapshot();
            (out, snap)
        };
        let (a, sa) = run("r-1").await;
        let (b, sb) = run("r-1").await;
        assert_eq!(a.report, b.report);
        assert_eq!(text(&a), text(&b));
        assert_eq!(a.manifest_sha256, b.manifest_sha256);
        // Every file but ops.json is the same; ops.json only measures.
        for (dir, files) in &sa.dirs {
            for (name, bytes) in files {
                if name != "ops.json" {
                    assert_eq!(Some(bytes), sb.dirs[dir].get(name), "{dir}/{name}");
                }
            }
        }
        let (c, _) = run("r-2").await;
        let shas = |o: &ReplayOutcome| -> BTreeMap<String, String> {
            o.report
                .cases
                .iter()
                .map(|r| (r.id.clone(), r.portfolio_sha256.clone()))
                .collect()
        };
        assert_eq!(shas(&a), shas(&c));
        // Recorded drafts went through the tools' checks, none refused.
        assert!(a
            .report
            .cases
            .iter()
            .all(|r| r.stages == StageSource::Recorded && r.refused_drafts == 0));
    }

    /// A refused recorded draft fails its stage, never the replay; with no
    /// drafts and no runner the week holds.
    #[tokio::test]
    async fn refused_draft_and_no_runner_hold() {
        let bench = Bench::new();
        let mut set = set_of(&bench, &CASES[..1]);
        set.cases[0].proposals[0]["opportunity"]["signals"] =
            json!(["0000000000000000000000000000000000000000000000000000000000000001"]);
        let mut bare = set.cases[0].clone();
        bare.id = "bare".into();
        bare.proposals.clear();
        bare.challenges.clear();
        set.cases.push(bare);
        let out = replay(&bench, &set, &params("r-1", false)).await.unwrap();
        let (refused, bare) = (&out.report.cases[0], &out.report.cases[1]);
        assert_eq!(
            (refused.stages, refused.refused_drafts),
            (StageSource::Recorded, 1)
        );
        assert!(refused.answer.hold);
        assert!(out.markdown.contains("RECORDED (1 refused)"));
        assert_eq!(bare.stages, StageSource::None);
        assert!(bare.answer.hold && bare.held.is_empty());
    }
}
