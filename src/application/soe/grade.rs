//! The operator's answers on a frozen live cycle (O4 "operator grading",
//! "forecast log"): grades and forecast resolutions. Both are append-only
//! state logs — a regrade or a resolution is a new line, an earlier line is
//! never rewritten.
//!
//! | Use case | Writes |
//! |---|---|
//! | [`grade_cycle`] | the operator's `soe.cycle_grade/1` file, checked → one line of `grades.jsonl` |
//! | [`grades`] · [`current_grades`] | every grade line, in order · the latest version per cycle (the cycle's grade) |
//! | [`resolve_cycle`] | resolved forecast items → `resolutions.jsonl` (`soe.forecast_resolution/1`, one line each) |
//! | [`resolutions`] | every resolution line, in order |
//!
//! | [`grade_cycle`] check (every refusal listed; nothing written) | Code |
//! |---|---|
//! | the record: header, the six grades 1..=5, `graded_at` known | the record's codes |
//! | `cycle_id` names a frozen live cycle (`cycles/<id>/` with `MANIFEST.json`) | `cycle_not_frozen` |
//! | `graded_at` surely after the cycle's decision | `invalid_time` |
//! | a `FALSE_POSITIVE` names a candidate the cycle decided | `unknown_target` |
//! | a first grade is version 1; a regrade keeps the id, is the latest version + 1 and is not graded before it; another id for a graded cycle | `stale_grade` · `duplicate` |
//!
//! | [`resolve_cycle`] rule | Value |
//! |---|---|
//! | Cycle | a frozen live cycle; its `forecast.json` (its sha256 goes on every line) |
//! | Once | an item resolved once stays resolved; an answer for it is `duplicate` |
//! | Operator answers | [`Answers`] (`cycle_id` = the cycle, `[[resolutions]]` = `forecast::Resolution`), each through `forecast::resolve` (evidence surely after the freeze, a hit by `resolve_by`, a miss after it); any refusal refuses the call |
//! | `EVIDENCE_APPEARS` without an answer | the source store's `knowable` view at now, that event only: the first record knowable surely after `frozen_at` → a hit at that time when not after `resolve_by` (the record ids by then as evidence); none by `resolve_by` and the deadline passed → a miss checked now; else pending |
//! | `ASSUMPTION_WITHIN` · `OPERATOR_RESOLVES` without an answer | pending — only the operator resolves them |

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::cycle::{DECIDED, FORECAST};
use super::submit::{head, read_json};
use crate::application::sources::{evidence_as_of, AsOfRequest};
use crate::config::sources::SourcesConfig;
use crate::domain::canonical::{canonical_json, sha256_hex};
use crate::domain::lineage::value::{valid_id, Time, TimeOrder};
use crate::domain::soe::forecast::{resolve, Forecast, Observable, Resolution};
use crate::domain::soe::record::{from_json, from_toml};
use crate::domain::soe::review::{CycleGrade, MissKind};
use crate::domain::soe::value::{codes, Bps, ValueError};
use crate::domain::source::AsOfMode;
use crate::ports::soe::{CycleStore, RunDir, RunStatus, StateLog};
use crate::ports::source_store::SourceStore;

pub(crate) const CYCLE_NOT_FROZEN: &str = "cycle_not_frozen";
pub(crate) const STALE_GRADE: &str = "stale_grade";
/// A resolution line's schema.
pub(crate) const RESOLUTION_SCHEMA: &str = "soe.forecast_resolution/1";

/// `resolved_by` of an item the source store resolved.
pub(crate) const BY_EVIDENCE: &str = "evidence";

/// The frozen live cycle `id`'s decision time, or why it is not one.
fn frozen_cycle(store: &dyn CycleStore, id: &str) -> Result<std::result::Result<i64, ValueError>> {
    if !valid_id(id) {
        return Ok(Err(ValueError::new(
            codes::INVALID_ID,
            format!("cycle_id: `{id}` is not an id"),
        )));
    }
    let dir = RunDir::Cycle(id.to_string());
    match store.status(&dir)? {
        RunStatus::Frozen => Ok(Ok(head(store, &dir)?.decided_at_ms)),
        s => Ok(Err(ValueError::new(
            CYCLE_NOT_FROZEN,
            format!(
                "cycle_id: {dir} is {} — only a frozen live cycle is graded or resolved",
                match s {
                    RunStatus::Absent => "absent",
                    _ => "claimed and never frozen",
                }
            ),
        ))),
    }
}

/// One state log, each line parsed by `parse`; an error names the line.
fn parsed<T>(
    store: &dyn CycleStore,
    log: StateLog,
    parse: impl Fn(&str) -> std::result::Result<T, String>,
) -> Result<Vec<T>> {
    store
        .lines(log)?
        .iter()
        .enumerate()
        .map(|(i, l)| {
            parse(l).map_err(|e| anyhow::anyhow!("{} line {}: {e}", log.file_name(), i + 1))
        })
        .collect()
}

/// Every grade line, in order.
pub(crate) fn grades(store: &dyn CycleStore) -> Result<Vec<CycleGrade>> {
    parsed(store, StateLog::Grades, |l| {
        from_json::<CycleGrade>(l).map_err(|e| format!("{e:?}"))
    })
}

/// The latest version of each cycle's grade.
pub(crate) fn current_grades(store: &dyn CycleStore) -> Result<BTreeMap<String, CycleGrade>> {
    let mut out: BTreeMap<String, CycleGrade> = BTreeMap::new();
    for g in grades(store)? {
        match out.get(&g.cycle_id) {
            Some(prev) if prev.version >= g.version => {}
            _ => {
                out.insert(g.cycle_id.clone(), g);
            }
        }
    }
    Ok(out)
}

/// What [`grade_cycle`] appended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Graded {
    pub grade: CycleGrade,
    /// Its 1-based line in `grades.jsonl`.
    pub line: u64,
    /// The version it supersedes (a regrade).
    pub supersedes: Option<u32>,
}

/// The candidate ids a frozen cycle decided (`decided.json`).
fn decided_ids(store: &dyn CycleStore, dir: &RunDir) -> Result<BTreeSet<String>> {
    #[derive(Deserialize)]
    struct Row {
        candidate: String,
    }
    #[derive(Deserialize)]
    struct Decided {
        candidates: Vec<Row>,
    }
    let d: Decided = read_json(store, dir, DECIDED)?
        .with_context(|| format!("{dir}/{DECIDED}: missing in a frozen cycle"))?;
    Ok(d.candidates.into_iter().map(|r| r.candidate).collect())
}

/// Module table: check and append one grade (`text` = the operator's TOML).
pub(crate) fn grade_cycle(
    store: &dyn CycleStore,
    text: &str,
) -> Result<std::result::Result<Graded, Vec<ValueError>>> {
    let g = match from_toml::<CycleGrade>(text) {
        Ok(g) => g,
        Err(e) => return Ok(Err(e)),
    };
    let decided_at_ms = match frozen_cycle(store, &g.cycle_id)? {
        Ok(t) => t,
        Err(e) => return Ok(Err(vec![e])),
    };
    let dir = RunDir::Cycle(g.cycle_id.clone());
    let mut errors = Vec::new();
    let decision = Time::At(decided_at_ms);
    if g.graded_at.order(&decision) != TimeOrder::After {
        errors.push(ValueError::new(
            codes::INVALID_TIME,
            format!(
                "graded_at: {} is not surely after the cycle's decision {decision}",
                g.graded_at
            ),
        ));
    }
    let ids = decided_ids(store, &dir)?;
    for (i, m) in g.misses.iter().enumerate() {
        if m.kind == MissKind::FalsePositive && !ids.contains(&m.candidate) {
            errors.push(ValueError::new(
                codes::UNKNOWN_TARGET,
                format!(
                    "misses[{i}].candidate: `{}` is not a candidate {dir} decided — a false positive is one it decided",
                    m.candidate
                ),
            ));
        }
    }
    let prev = current_grades(store)?.remove(&g.cycle_id);
    let supersedes = match &prev {
        None if g.version != 1 => {
            errors.push(ValueError::new(
                STALE_GRADE,
                format!(
                    "version: {} — the cycle's first grade is version 1",
                    g.version
                ),
            ));
            None
        }
        None => None,
        Some(p) => {
            if p.id != g.id {
                errors.push(ValueError::new(
                    codes::DUPLICATE,
                    format!(
                        "id: {} is graded as `{}` already — a regrade keeps that id",
                        g.cycle_id, p.id
                    ),
                ));
            }
            if g.version != p.version + 1 {
                errors.push(ValueError::new(
                    STALE_GRADE,
                    format!(
                        "version: {} — the latest grade is version {}, a regrade is version {}",
                        g.version,
                        p.version,
                        p.version + 1
                    ),
                ));
            }
            if g.graded_at.order(&p.graded_at) == TimeOrder::NotAfter && g.graded_at != p.graded_at
            {
                errors.push(ValueError::new(
                    codes::INVALID_TIME,
                    format!(
                        "graded_at: {} is before the version it supersedes ({})",
                        g.graded_at, p.graded_at
                    ),
                ));
            }
            Some(p.version)
        }
    };
    if !errors.is_empty() {
        return Ok(Err(errors));
    }
    let line = store.append_line(
        StateLog::Grades,
        &canonical_json(&serde_json::to_value(&g)?),
    )?;
    Ok(Ok(Graded {
        grade: g,
        line,
        supersedes,
    }))
}

// ---------------------------------------------------------------------------
// Forecast resolution
// ---------------------------------------------------------------------------

/// The operator's answers for one cycle's forecast (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Answers {
    pub cycle_id: String,
    #[serde(default)]
    pub resolutions: Vec<Resolution>,
}

/// One `resolutions.jsonl` line (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolutionLine {
    pub schema: String,
    pub cycle_id: String,
    /// sha256 of the frozen `forecast.json` text.
    pub forecast_sha256: String,
    pub item: usize,
    pub candidate: String,
    pub probability: Bps,
    pub hit: bool,
    pub observed_at: Time,
    pub evidence: Vec<String>,
    pub resolved_by: String,
    pub resolved_at_ms: i64,
}

/// Every resolution line, in order.
pub(crate) fn resolutions(store: &dyn CycleStore) -> Result<Vec<ResolutionLine>> {
    parsed(store, StateLog::Resolutions, |l| {
        serde_json::from_str::<ResolutionLine>(l).map_err(|e| e.to_string())
    })
}

/// What [`resolve_cycle`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Resolve {
    pub cycle_id: String,
    pub forecast_sha256: String,
    pub items: usize,
    /// Appended now, in item order.
    pub added: Vec<ResolutionLine>,
    /// Items resolved before.
    pub already: Vec<usize>,
    /// Items still open, with why.
    pub pending: Vec<(usize, String)>,
}

/// The frozen forecast of `cycle_id` and the sha256 of its text.
pub(crate) fn frozen_forecast(
    store: &dyn CycleStore,
    cycle_id: &str,
) -> Result<(Forecast, String)> {
    let dir = RunDir::Cycle(cycle_id.to_string());
    let bytes = store
        .read(&dir, FORECAST)?
        .with_context(|| format!("{dir}/{FORECAST}: missing in a frozen cycle"))?;
    let text = std::str::from_utf8(&bytes).with_context(|| format!("{dir}/{FORECAST}"))?;
    let f = from_json::<Forecast>(text.trim_end())
        .map_err(|e| anyhow::anyhow!("{dir}/{FORECAST}: {e:?}"))?;
    Ok((f, sha256_hex(text.trim_end())))
}

/// The store's answer for an `EVIDENCE_APPEARS` item (module table).
async fn from_evidence(
    sources: Option<(&dyn SourceStore, &SourcesConfig)>,
    event_key: &str,
    item: usize,
    f: &Forecast,
    now_ms: i64,
) -> Result<std::result::Result<Resolution, String>> {
    let Some((store, registry)) = sources else {
        return Ok(Err("no source store given (--sandbox)".into()));
    };
    let x = &f.items[item].item;
    let (Some(frozen), Some(by)) = (f.frozen_at.latest(), x.resolve_by.latest()) else {
        return Ok(Err("frozen_at or resolve_by unknown".into()));
    };
    let packet = evidence_as_of(
        Some(store),
        registry,
        &AsOfRequest {
            at_ms: now_ms,
            mode: AsOfMode::Knowable,
            source: None,
            entity: None,
            event_key: Some(event_key.to_string()),
            published_from_ms: None,
            published_to_ms: None,
        },
    )
    .await?;
    let mut later: Vec<(i64, &str)> = packet
        .facts
        .iter()
        .chain(&packet.pending)
        .chain(&packet.expired)
        .filter(|r| r.event_key == event_key && r.visible_ms > frozen)
        .map(|r| (r.visible_ms, r.record_id.as_str()))
        .collect();
    later.sort();
    let resolution = |hit: bool, at: i64, evidence: Vec<String>| Resolution {
        item,
        hit,
        observed_at: Time::At(at),
        evidence,
        resolved_by: BY_EVIDENCE.into(),
    };
    match later.first() {
        Some(&(first, _)) if first <= by => {
            let ids = later
                .iter()
                .filter(|(t, _)| *t <= by)
                .map(|(_, id)| id.to_string())
                .collect();
            Ok(Ok(resolution(true, first, ids)))
        }
        _ if now_ms > by => {
            let late = later.iter().map(|(_, id)| id.to_string()).collect();
            Ok(Ok(resolution(false, now_ms, late)))
        }
        _ => Ok(Err(format!(
            "no record of `{event_key}` knowable after the freeze yet; due by {}",
            x.resolve_by
        ))),
    }
}

/// Module table: resolve what can be resolved of `cycle_id`'s forecast.
pub(crate) async fn resolve_cycle(
    store: &dyn CycleStore,
    sources: Option<(&dyn SourceStore, &SourcesConfig)>,
    cycle_id: &str,
    answers: Option<&Answers>,
    now_ms: i64,
) -> Result<std::result::Result<Resolve, Vec<ValueError>>> {
    if let Err(e) = frozen_cycle(store, cycle_id)? {
        return Ok(Err(vec![e]));
    }
    let (f, forecast_sha256) = frozen_forecast(store, cycle_id)?;
    let done: BTreeSet<usize> = resolutions(store)?
        .into_iter()
        .filter(|l| l.cycle_id == cycle_id)
        .map(|l| l.item)
        .collect();
    let mut errors = Vec::new();
    let mut given: BTreeMap<usize, &Resolution> = BTreeMap::new();
    if let Some(a) = answers {
        if a.cycle_id != cycle_id {
            errors.push(ValueError::new(
                codes::INVALID_FIELD,
                format!(
                    "cycle_id: the answers are for `{}`, not `{cycle_id}`",
                    a.cycle_id
                ),
            ));
        }
        for (i, r) in a.resolutions.iter().enumerate() {
            if done.contains(&r.item) {
                errors.push(ValueError::new(
                    codes::DUPLICATE,
                    format!(
                        "resolutions[{i}]: item {} is resolved already — a resolution is never rewritten",
                        r.item
                    ),
                ));
            } else if given.insert(r.item, r).is_some() {
                errors.push(ValueError::new(
                    codes::DUPLICATE,
                    format!("resolutions[{i}]: item {} answered twice", r.item),
                ));
            }
        }
    }
    let mut out = Resolve {
        cycle_id: cycle_id.to_string(),
        forecast_sha256: forecast_sha256.clone(),
        items: f.items.len(),
        added: Vec::new(),
        already: done.iter().copied().collect(),
        pending: Vec::new(),
    };
    let mut chosen: Vec<Resolution> = Vec::new();
    for (&item, r) in &given {
        if item >= f.items.len() {
            // `resolve` names it (`unknown_item`).
            chosen.push((*r).clone());
        }
    }
    for (item, x) in f.items.iter().enumerate() {
        if done.contains(&item) {
            continue;
        }
        if let Some(r) = given.get(&item) {
            chosen.push((*r).clone());
            continue;
        }
        match &x.item.observable {
            Observable::EvidenceAppears { event_key } => {
                match from_evidence(sources, event_key, item, &f, now_ms).await? {
                    Ok(r) => chosen.push(r),
                    Err(why) => out.pending.push((item, why)),
                }
            }
            Observable::AssumptionWithin { field, .. } => out
                .pending
                .push((item, format!("`{field}`: the operator resolves it"))),
            Observable::OperatorResolves { .. } => out
                .pending
                .push((item, "the operator resolves it".to_string())),
        }
    }
    chosen.sort_by_key(|r| r.item);
    let mut lines = Vec::new();
    for r in &chosen {
        match resolve(&f, r) {
            Ok(v) => lines.push(ResolutionLine {
                schema: RESOLUTION_SCHEMA.into(),
                cycle_id: cycle_id.to_string(),
                forecast_sha256: forecast_sha256.clone(),
                item: v.item,
                candidate: v.candidate,
                probability: v.probability,
                hit: v.hit,
                observed_at: r.observed_at,
                evidence: r.evidence.clone(),
                resolved_by: r.resolved_by.clone(),
                resolved_at_ms: now_ms,
            }),
            Err(e) => errors.push(ValueError::new(
                e.code,
                format!("item {}: {}", r.item, e.message),
            )),
        }
    }
    if !errors.is_empty() {
        return Ok(Err(errors));
    }
    for l in &lines {
        store.append_line(
            StateLog::Resolutions,
            &canonical_json(&serde_json::to_value(l)?),
        )?;
    }
    out.added = lines;
    Ok(Ok(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::soe::cycle::Target;
    use crate::application::soe::tests::{load_case, Bench, MIN};
    use crate::domain::source::testkit::{rec, Src};

    fn grade_toml(cycle: &str, id: &str, version: u32, graded_at: &str, extra: &str) -> String {
        format!(
            r#"schema = "soe.cycle_grade/1"
id = "{id}"
version = {version}
cycle_id = "{cycle}"
graded_by = "operator"
graded_at = "{graded_at}"
relevance = 3
evidence = 4
economics = 3
hidden_labor = 2
novelty_bias = 4
actionability = 2
correction_minutes = 25
research_hours = 1
changed_decision = false
misses = []
{extra}"#
        )
    }

    fn codes_of<T>(r: Result<std::result::Result<T, Vec<ValueError>>>) -> Vec<&'static str> {
        match r.unwrap() {
            Ok(_) => Vec::new(),
            Err(e) => e.iter().map(|x| x.code).collect(),
        }
    }

    /// Only a frozen live cycle takes a grade: absent, claimed-never-frozen
    /// and replay runs are refused, nothing written.
    #[tokio::test]
    async fn unfrozen_cycle_refused() {
        let bench = Bench::new();
        let g = grade_toml("2026-W41", "g-41", 1, "2026-10-06T09:00:00Z", "");
        assert_eq!(codes_of(grade_cycle(&*bench.store, &g)), [CYCLE_NOT_FROZEN]);
        // Claimed, never frozen.
        bench
            .store
            .claim(&RunDir::Cycle("2026-W41".into()))
            .unwrap();
        assert_eq!(codes_of(grade_cycle(&*bench.store, &g)), [CYCLE_NOT_FROZEN]);
        // A frozen replay is not a live cycle.
        let case = load_case("strong_news_weak_demand");
        let (out, _) = bench
            .run_case(
                &case,
                Target::Replay {
                    run_id: "r-grade".into(),
                },
            )
            .await;
        let r = grade_toml(out.dir.id(), "g-r", 1, "2026-10-06T09:00:00Z", "");
        assert_eq!(codes_of(grade_cycle(&*bench.store, &r)), [CYCLE_NOT_FROZEN]);
        // A broken record lists its problems first.
        let broken = g.replace("relevance = 3", "relevance = 9");
        assert_eq!(
            codes_of(grade_cycle(&*bench.store, &broken)),
            [codes::INVALID_FIELD]
        );
        assert!(bench.store.lines(StateLog::Grades).unwrap().is_empty());
        // Resolving is refused the same way.
        let res = resolve_cycle(&*bench.store, None, "2026-W41", None, 0).await;
        assert_eq!(codes_of(res), [CYCLE_NOT_FROZEN]);
    }

    /// A regrade is a new line with the next version; the earlier line
    /// stays byte for byte; the latest version is the cycle's grade.
    #[tokio::test]
    async fn regrade_appends_supersedes() {
        let case = load_case("strong_news_weak_demand");
        let bench = Bench::new();
        bench.run_case(&case, Target::Cycle).await;
        let s = &*bench.store;
        let v1 = grade_toml("2026-W41", "g-41", 1, "2026-10-06T09:00:00Z", "");
        let first = grade_cycle(s, &v1).unwrap().unwrap();
        assert_eq!((first.line, first.supersedes), (1, None));
        let line1 = s.lines(StateLog::Grades).unwrap()[0].clone();
        // The same version again, a version gap, another id, before the decision.
        assert_eq!(codes_of(grade_cycle(s, &v1)), [STALE_GRADE]);
        let gap = grade_toml("2026-W41", "g-41", 3, "2026-10-07T09:00:00Z", "");
        assert_eq!(codes_of(grade_cycle(s, &gap)), [STALE_GRADE]);
        let other = grade_toml("2026-W41", "g-other", 2, "2026-10-07T09:00:00Z", "");
        assert_eq!(codes_of(grade_cycle(s, &other)), [codes::DUPLICATE]);
        let early = grade_toml("2026-W41", "g-41", 2, "2026-10-05T11:00:00Z", "");
        assert_eq!(
            codes_of(grade_cycle(s, &early)),
            [codes::INVALID_TIME, codes::INVALID_TIME]
        );
        // A false positive must be a decided candidate.
        let fp = grade_toml(
            "2026-W41",
            "g-41",
            2,
            "2026-10-07T09:00:00Z",
            "\n[[misses]]\nkind = \"FALSE_POSITIVE\"\ncandidate = \"nobody\"\nclass = \"EVIDENCE\"\n",
        )
        .replace("misses = []\n", "");
        assert_eq!(codes_of(grade_cycle(s, &fp)), [codes::UNKNOWN_TARGET]);
        // The regrade.
        let v2 = grade_toml("2026-W41", "g-41", 2, "2026-10-07T09:00:00Z", "")
            .replace("correction_minutes = 25", "correction_minutes = 10");
        let second = grade_cycle(s, &v2).unwrap().unwrap();
        assert_eq!((second.line, second.supersedes), (2, Some(1)));
        let lines = s.lines(StateLog::Grades).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], line1, "an earlier grade is never rewritten");
        let current = current_grades(s).unwrap();
        assert_eq!(current["2026-W41"].version, 2);
        assert_eq!(current["2026-W41"].correction_minutes, 10);
        assert_eq!(grades(s).unwrap().len(), 2);
    }

    fn t(s: &str) -> i64 {
        s.parse::<Time>().unwrap().earliest().unwrap()
    }

    /// The weekly-rerun week: item 0 = `demand-automation` waits for the
    /// post `p-71` (`EVIDENCE_APPEARS`, by 2026-10-26), item 1 =
    /// `steady-automation` (`OPERATOR_RESOLVES`, by 2026-11-02).
    async fn w41() -> Bench {
        let bench = Bench::new();
        bench
            .run_case(&load_case("weekly_rerun_w41"), Target::Cycle)
            .await;
        let (f, _) = frozen_forecast(&*bench.store, "2026-W41").unwrap();
        let kinds: Vec<(&str, bool)> = f
            .items
            .iter()
            .map(|x| {
                (
                    x.candidate.as_str(),
                    matches!(x.item.observable, Observable::EvidenceAppears { .. }),
                )
            })
            .collect();
        assert_eq!(
            kinds,
            [("demand-automation", true), ("steady-automation", false)]
        );
        bench
    }

    fn answer(item: usize, hit: bool, observed: &str) -> Resolution {
        Resolution {
            item,
            hit,
            observed_at: observed.parse().unwrap(),
            evidence: vec![],
            resolved_by: "operator".into(),
        }
    }

    /// `EVIDENCE_APPEARS` resolves from a record knowable after the freeze;
    /// an operator answer resolves the rest; nothing resolves twice.
    #[tokio::test]
    async fn resolve_from_later_evidence() {
        let bench = w41().await;
        let s = &*bench.store;
        let (_, sha) = frozen_forecast(s, "2026-W41").unwrap();
        let sources = Some((&bench.sources as &dyn SourceStore, &bench.registry));
        // Nothing new yet, the deadline ahead: both pending.
        let r = resolve_cycle(s, sources, "2026-W41", None, t("2026-10-06T00:00:00Z"))
            .await
            .unwrap()
            .unwrap();
        assert!(r.added.is_empty(), "{r:?}");
        assert_eq!(r.pending.iter().map(|p| p.0).collect::<Vec<_>>(), [0, 1]);
        // The post arrives after the freeze, before the deadline: a hit.
        let p = t("2026-10-08T12:00:00Z");
        let post = rec(Src::FORUM, "p-71", p, p + 5 * MIN, p + 6 * MIN);
        bench.sources.add(std::slice::from_ref(&post));
        let r = resolve_cycle(s, sources, "2026-W41", None, t("2026-10-09T00:00:00Z"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.added.len(), 1, "{r:?}");
        let l = &r.added[0];
        assert_eq!(
            (l.item, l.hit, l.resolved_by.as_str()),
            (0, true, BY_EVIDENCE)
        );
        assert_eq!(l.candidate, "demand-automation");
        assert_eq!(l.forecast_sha256, sha);
        assert_eq!(l.evidence, std::slice::from_ref(&post.record_id));
        assert!(matches!(l.observed_at, Time::At(x) if x >= p && x <= p + 5 * MIN));
        assert_eq!(r.pending.iter().map(|p| p.0).collect::<Vec<_>>(), [1]);
        let after_hit = s.lines(StateLog::Resolutions).unwrap();
        // Never twice: a rerun adds nothing; an answer for item 0 is refused.
        let late = t("2026-10-27T00:00:00Z");
        let again = resolve_cycle(s, sources, "2026-W41", None, late)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((again.added.len(), again.already.clone()), (0, vec![0]));
        let dup = Answers {
            cycle_id: "2026-W41".into(),
            resolutions: vec![answer(0, false, "2026-10-27T00:00:00Z")],
        };
        let refused = resolve_cycle(s, sources, "2026-W41", Some(&dup), late).await;
        assert_eq!(codes_of(refused), [codes::DUPLICATE]);
        assert_eq!(s.lines(StateLog::Resolutions).unwrap(), after_hit);
        // The operator resolves item 1.
        let ok = Answers {
            cycle_id: "2026-W41".into(),
            resolutions: vec![answer(1, true, "2026-10-20T09:00:00Z")],
        };
        let r = resolve_cycle(s, sources, "2026-W41", Some(&ok), late)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.added.len(), 1);
        assert_eq!(
            (r.added[0].item, r.added[0].resolved_by.as_str()),
            (1, "operator")
        );
        let all = resolutions(s).unwrap();
        assert_eq!(all.iter().map(|l| l.item).collect::<Vec<_>>(), [0, 1]);
    }

    /// Without the event the deadline passing is a miss; every operator
    /// answer goes through `forecast::resolve`; a refusal writes nothing.
    #[tokio::test]
    async fn resolve_miss_and_operator_answers() {
        let bench = w41().await;
        let s = &*bench.store;
        let late = t("2026-10-27T00:00:00Z");
        let bad = |r: Resolution| Answers {
            cycle_id: "2026-W41".into(),
            resolutions: vec![r],
        };
        for (r, code) in [
            (
                answer(1, true, "2026-10-05T11:00:00Z"),
                codes::EVIDENCE_BEFORE_FREEZE,
            ),
            (
                answer(1, false, "2026-10-20T09:00:00Z"),
                codes::MISS_BEFORE_DEADLINE,
            ),
            (answer(7, true, "2026-10-20T09:00:00Z"), codes::UNKNOWN_ITEM),
        ] {
            let got = resolve_cycle(s, None, "2026-W41", Some(&bad(r)), late).await;
            assert_eq!(codes_of(got), [code]);
        }
        let wrong = Answers {
            cycle_id: "2026-W42".into(),
            resolutions: vec![],
        };
        let got = resolve_cycle(s, None, "2026-W41", Some(&wrong), late).await;
        assert_eq!(codes_of(got), [codes::INVALID_FIELD]);
        assert!(s.lines(StateLog::Resolutions).unwrap().is_empty());
        // No store: the evidence item waits; with it, past the deadline, a miss.
        let r = resolve_cycle(s, None, "2026-W41", None, late)
            .await
            .unwrap()
            .unwrap();
        assert!(r.pending[0].1.contains("--sandbox"), "{r:?}");
        let sources = Some((&bench.sources as &dyn SourceStore, &bench.registry));
        let r = resolve_cycle(s, sources, "2026-W41", None, late)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.added.len(), 1, "{r:?}");
        assert!(!r.added[0].hit);
        assert_eq!(r.added[0].observed_at, Time::At(late));
        assert_eq!(resolutions(s).unwrap(), r.added);
    }
}
