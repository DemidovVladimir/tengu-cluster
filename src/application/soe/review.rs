//! Operator Review #2 packet (roadmap O4 "Operator Review #2 packet", § 14
//! stop conditions, § 15; PRD § 11): the frozen live cycles of a period,
//! their grades and resolved forecasts, the replays and the state's
//! integrity, assembled by `domain::soe::review::ReviewPacket::of` and
//! written once under `reviews/<id>/`. The packet enables a decision and
//! makes none; its last line is the STOP ([`STOP_LINE`]): Operator Review #2
//! decides, and nothing of O5–O8 exists to run.
//!
//! | Step | Reads · writes | Rule |
//! |---|---|---|
//! | Cycles | `cycles/<id>/`, frozen, `from ..= to` (ISO weeks), oldest first | a claimed-never-frozen cycle is listed `UNFINISHED`, never reviewed |
//! | Per cycle | `portfolio.json`, `decided.json`, its proposals (`proposals.jsonl` + `carried.json`), `packet.json`, `ops.json`, `forecast.json`, the latest grade (`grades.jsonl`) | `review::CycleInput`: candidate → mechanism, unsupported claims of the week's proposals, one `SourceTerms` per packet source (stated only when every citation of it states its terms) |
//! | Forecasts | `resolutions.jsonl` | the cycles' resolved items → calibration; the rest pending; a resolution of another forecast text (`forecast_sha256`) is an integrity finding |
//! | Replays | `replays.jsonl` | per run: set, cases, the development summary, the holdout read line — only what the replay counted |
//! | Integrity | `freeze::verify_state` | every run dir, the forecast chain, each cycle's log line — shown, never hidden; not intact ⇒ `integrity_ok = false` |
//! | Write | `reviews/<id>/packet.json` (`soe.review_packet/1`, canonical), `packet.md`, frozen; one `reviews.jsonl` line | `<id>` = the UTC day of the clock, then `.2`, `.3`, … when taken |
//!
//! | `packet.md` § (roadmap O4) | Decision it enables |
//! |---|---|
//! | 1 Weekly portfolios and grades | is the output consistently useful? |
//! | 2 Forecast vs later evidence | is confidence calibrated? |
//! | 3 Missed / false opportunities | which failure class dominates? |
//! | 4 Cost and operator hours | does the system save scarce time? |
//! | 5 Source coverage and rights | is expansion justified and lawful? |
//! | 6 Best candidate lane | O6A service / integration · O6B acquisition / partnership |
//! | then | replays, integrity, roadmap § 14 stop flags, the three choices, [`STOP_LINE`] |

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::cycle::{DECIDED, PORTFOLIO};
use super::freeze::{freeze, verify_state, LogState, StateCheck, OPS};
use super::grade::{current_grades, frozen_forecast, resolutions};
use super::submit::{carried, json_line, packet, read_json};
use crate::domain::canonical::canonical_json;
use crate::domain::soe::forecast::Resolved;
use crate::domain::soe::opportunity::Mechanism;
use crate::domain::soe::ops::CycleOps;
use crate::domain::soe::portfolio::{IsoWeek, WeeklyPortfolio};
use crate::domain::soe::proposal::MechanismProposal;
use crate::domain::soe::record::from_json;
use crate::domain::soe::review::{
    CycleGrade, CycleInput, Lane, ReviewPacket, SourceTerms, MIN_LIVE_CYCLES,
};
use crate::domain::source::EvidencePacket;
use crate::ports::clock::Clock;
use crate::ports::soe::{CycleStore, RunDir, RunStatus, StateLog};

pub(crate) const PACKET_JSON: &str = "packet.json";
pub(crate) const PACKET_MD: &str = "packet.md";
pub(crate) const REVIEW_SCHEMA: &str = "soe.review_packet/1";
/// The packet's last line (module doc).
pub(crate) const STOP_LINE: &str = "STOP — Operator Review #2 decides: STOP, REWORK, or authorize O5 with a maximum total validation budget and the exact allowed action kinds. Nothing from O5–O8 (validation experiments, contact, spend, publish, purchase, deploy) exists or runs before that decision.";

/// The period reviewed (inclusive ISO weeks; none = open-ended).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ReviewRange {
    pub from: Option<IsoWeek>,
    pub to: Option<IsoWeek>,
}

impl ReviewRange {
    fn holds(&self, w: IsoWeek) -> bool {
        self.from.map_or(true, |f| f <= w) && self.to.map_or(true, |t| w <= t)
    }
}

/// What [`build_review`] wrote.
#[derive(Debug, Clone)]
pub(crate) struct ReviewOutcome {
    pub dir: RunDir,
    /// `packet.json`.
    pub json: Value,
    pub markdown: String,
    pub manifest_sha256: String,
    /// The `reviews.jsonl` line.
    pub line: u64,
    pub integrity_ok: bool,
}

/// One frozen cycle, read (module table: Per cycle).
struct CycleData {
    id: String,
    portfolio: WeeklyPortfolio,
    mechanisms: BTreeMap<String, Mechanism>,
    unsupported: usize,
    sources: Vec<SourceTerms>,
    ops: CycleOps,
    forecast_items: usize,
    forecast_sha256: String,
}

#[derive(Deserialize)]
struct DecidedRow {
    candidate: String,
    proposal: String,
}

#[derive(Deserialize)]
struct DecidedFile {
    candidates: Vec<DecidedRow>,
}

fn required<T: serde::de::DeserializeOwned>(
    store: &dyn CycleStore,
    dir: &RunDir,
    name: &str,
) -> Result<T> {
    read_json(store, dir, name)?.with_context(|| format!("{dir}/{name}: missing in a frozen cycle"))
}

/// One `SourceTerms` per source of the packet (module table).
fn packet_sources(p: &EvidencePacket) -> Vec<SourceTerms> {
    let source_of: BTreeMap<&str, &str> = p
        .facts
        .iter()
        .chain(&p.pending)
        .chain(&p.expired)
        .map(|r| (r.record_id.as_str(), r.source_id.as_str()))
        .chain(
            p.withdrawn
                .iter()
                .map(|r| (r.record_id.as_str(), r.source_id.as_str())),
        )
        .chain(
            p.unparsed
                .iter()
                .map(|r| (r.record_id.as_str(), r.source_id.as_str())),
        )
        .collect();
    let mut by_source: BTreeMap<&str, SourceTerms> = BTreeMap::new();
    for c in &p.citations {
        let Some(source) = source_of.get(c.record_id.as_str()) else {
            continue;
        };
        let t = SourceTerms {
            source_id: source.to_string(),
            license_or_terms: c.license_or_terms.clone(),
            terms_sha256: c.terms_sha256.clone(),
        };
        match by_source.get(source) {
            // An unstated citation wins: stated only when every one is.
            Some(prev) if !prev.stated() || t.stated() => {}
            _ => {
                by_source.insert(source, t);
            }
        }
    }
    by_source.into_values().collect()
}

fn read_cycle(store: &dyn CycleStore, id: &str) -> Result<CycleData> {
    let dir = RunDir::Cycle(id.to_string());
    let text = store
        .read(&dir, PORTFOLIO)?
        .with_context(|| format!("{dir}/{PORTFOLIO}: missing in a frozen cycle"))?;
    let portfolio: WeeklyPortfolio =
        from_json(std::str::from_utf8(&text).context(PORTFOLIO)?.trim_end())
            .map_err(|e| anyhow::anyhow!("{dir}/{PORTFOLIO}: {e:?}"))?;
    let decided: DecidedFile = required(store, &dir, DECIDED)?;
    let mut records: BTreeMap<String, MechanismProposal> = store
        .proposals(&dir)?
        .into_iter()
        .map(|p| (p.id.clone(), p))
        .collect();
    for p in carried(store, &dir)?.proposals {
        records.entry(p.id.clone()).or_insert(p);
    }
    let mut mechanisms = BTreeMap::new();
    let mut unsupported = 0;
    for r in &decided.candidates {
        let p = records.get(&r.proposal).with_context(|| {
            format!(
                "{dir}/{DECIDED}: proposal `{}` of `{}` is in no record of the run",
                r.proposal, r.candidate
            )
        })?;
        mechanisms.insert(r.candidate.clone(), p.opportunity().mechanism);
        unsupported += p.draft.unsupported().len();
    }
    let (forecast, forecast_sha256) = frozen_forecast(store, id)?;
    Ok(CycleData {
        id: id.to_string(),
        portfolio,
        mechanisms,
        unsupported,
        sources: packet_sources(&packet(store, &dir)?),
        ops: required(store, &dir, OPS)?,
        forecast_items: forecast.items.len(),
        forecast_sha256,
    })
}

/// The free review id (module table: Write).
fn review_dir(store: &dyn CycleStore, now_ms: i64) -> Result<RunDir> {
    let day = chrono::DateTime::from_timestamp_millis(now_ms)
        .context("the clock is out of range")?
        .format("%Y-%m-%d")
        .to_string();
    for n in 1..=999 {
        let id = if n == 1 {
            day.clone()
        } else {
            format!("{day}.{n}")
        };
        let dir = RunDir::Review(id);
        if store.status(&dir)? == RunStatus::Absent {
            return Ok(dir);
        }
    }
    bail!("reviews/{day}: 999 reviews on one day — none left")
}

/// Module table: build, write and freeze one review packet.
pub(crate) fn build_review(
    store: &dyn CycleStore,
    clock: &dyn Clock,
    range: ReviewRange,
) -> Result<ReviewOutcome> {
    let now_ms = clock.now_ms();
    let mut cycles = Vec::new();
    let mut unfinished = Vec::new();
    for id in store.cycles()? {
        let Ok(week) = id.parse::<IsoWeek>() else {
            continue;
        };
        if !range.holds(week) {
            continue;
        }
        match store.status(&RunDir::Cycle(id.clone()))? {
            RunStatus::Frozen => cycles.push(read_cycle(store, &id)?),
            _ => unfinished.push(id),
        }
    }
    let grades: BTreeMap<String, CycleGrade> = current_grades(store)?;
    let ids: BTreeSet<&str> = cycles.iter().map(|c| c.id.as_str()).collect();
    let mut resolved: Vec<Resolved> = Vec::new();
    let mut stale_resolutions = Vec::new();
    let mut resolved_per: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for l in resolutions(store)? {
        let Some(c) = cycles.iter().find(|c| c.id == l.cycle_id) else {
            continue;
        };
        if l.forecast_sha256 != c.forecast_sha256 {
            stale_resolutions.push(format!(
                "{} item {}: resolved against forecast {} — the frozen forecast is {}",
                l.cycle_id, l.item, l.forecast_sha256, c.forecast_sha256
            ));
            continue;
        }
        let e = resolved_per.entry(l.cycle_id.clone()).or_default();
        e.0 += 1;
        e.1 += usize::from(l.hit);
        resolved.push(Resolved {
            candidate: l.candidate,
            item: l.item,
            probability: l.probability,
            hit: l.hit,
        });
    }
    let inputs: Vec<CycleInput> = cycles
        .iter()
        .map(|c| CycleInput {
            cycle_id: &c.id,
            portfolio: &c.portfolio,
            mechanisms: &c.mechanisms,
            unsupported_claims: c.unsupported,
            sources: &c.sources,
            ops: &c.ops,
            grade: grades.get(&c.id),
        })
        .collect();
    let packet = ReviewPacket::of(&inputs, &resolved);
    let check = verify_state(store)?;
    let integrity_ok = check.ok() && stale_resolutions.is_empty();
    let replays: Vec<Value> = store
        .lines(StateLog::Replays)?
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let forecasts: Vec<Value> = cycles
        .iter()
        .map(|c| {
            let (n, hits) = resolved_per.get(&c.id).copied().unwrap_or_default();
            json!({
                "cycle_id": c.id,
                "items": c.forecast_items,
                "resolved": n,
                "hits": hits,
                "pending": c.forecast_items.saturating_sub(n),
            })
        })
        .collect();
    let dir = review_dir(store, now_ms)?;
    let json = json!({
        "schema": REVIEW_SCHEMA,
        "review": dir.to_string(),
        "built_at_ms": now_ms,
        "range": {
            "from": range.from.map(|w| w.to_string()),
            "to": range.to.map(|w| w.to_string()),
        },
        "cycles": ids.iter().collect::<Vec<_>>(),
        "unfinished": unfinished,
        "min_live_cycles": MIN_LIVE_CYCLES,
        "packet": packet,
        "forecasts": forecasts,
        "replays": replays,
        "integrity": {
            "ok": integrity_ok,
            "state": check,
            "stale_resolutions": stale_resolutions,
        },
        "stop": STOP_LINE,
    });
    let markdown = render(&dir, &json, &packet, &check, &stale_resolutions);
    store.claim(&dir)?;
    store.write(&dir, PACKET_JSON, &json_line(&json)?)?;
    store.write(&dir, PACKET_MD, markdown.as_bytes())?;
    let manifest_sha256 = freeze(store, &dir)?;
    let line = store.append_line(
        StateLog::Reviews,
        &canonical_json(&json!({
            "schema": "soe.review_run/1",
            "run": dir.to_string(),
            "built_at_ms": now_ms,
            "cycles": ids.iter().collect::<Vec<_>>(),
            "integrity_ok": integrity_ok,
            "stop_flags": packet.stop_flags.iter().map(|f| f.code).collect::<Vec<_>>(),
            "manifest_sha256": manifest_sha256,
        })),
    )?;
    Ok(ReviewOutcome {
        dir,
        json,
        markdown,
        manifest_sha256,
        line,
        integrity_ok,
    })
}

// ---------------------------------------------------------------------------
// packet.md
// ---------------------------------------------------------------------------

fn name<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// `packet.md` (module table).
fn render(
    dir: &RunDir,
    json: &Value,
    p: &ReviewPacket,
    check: &StateCheck,
    stale: &[String],
) -> String {
    let mut out = format!(
        "# SOE Operator Review #2 packet `{dir}`\n\nThe packet enables a decision; it makes none.\n\n| Field | Value |\n|---|---|\n| Cycles | {} frozen ({}) · {} graded · enough cycles (≥ {MIN_LIVE_CYCLES}): {} |\n| Unfinished | {} |\n| Integrity | {} |\n",
        p.cycles.len(),
        p.cycles
            .iter()
            .map(|c| c.cycle_id.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        p.graded,
        if p.enough_cycles { "yes" } else { "NO" },
        json["unfinished"]
            .as_array()
            .filter(|a| !a.is_empty())
            .map_or("none".to_string(), |a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            }),
        if check.ok() && stale.is_empty() {
            "intact (every run dir MATCH, forecast chain intact, every cycle on the log)"
        } else {
            "NOT INTACT — see Integrity"
        }
    );

    out.push_str("\n## 1. Weekly portfolios and grades\n\nIs the output consistently useful?\n\n| Cycle | Verdict | Ranked · held · rejected | Tests | Owner h · cash | Unsupported claims | Relevance · evidence · economics · hidden labor · novelty bias · actionability | Changed a decision |\n|---|---|---|---|---|---|---|---|\n");
    for c in &p.cycles {
        let grade = c.grade.as_ref().map_or("not graded".to_string(), |g| {
            [
                "relevance",
                "evidence",
                "economics",
                "hidden_labor",
                "novelty_bias",
                "actionability",
            ]
            .iter()
            .map(|k| g.get(k).map_or("-".to_string(), u8::to_string))
            .collect::<Vec<_>>()
            .join(" · ")
        });
        let _ = writeln!(
            out,
            "| `{}` | {} | {} · {} · {} | {} | {} · {} | {} | {grade} | {} |",
            c.cycle_id,
            if c.hold { "HOLD" } else { "RANKED" },
            c.ranked,
            c.held,
            c.rejected,
            c.tests,
            c.owner_hours,
            c.cash,
            c.unsupported_claims,
            c.changed_decision
                .map_or("-", |b| if b { "yes" } else { "no" })
        );
    }

    out.push_str("\n## 2. Forecast vs later evidence\n\nIs confidence calibrated?\n\n| Cycle | Items | Resolved | Hits | Pending |\n|---|---|---|---|---|\n");
    for f in json["forecasts"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} | {} |",
            f["cycle_id"].as_str().unwrap_or_default(),
            f["items"],
            f["resolved"],
            f["hits"],
            f["pending"]
        );
    }
    let cal = &p.calibration;
    let _ = writeln!(
        out,
        "\nResolved items: {} · Brier ×10⁸: {}\n\n| Predicted (bps) | Items | Hits | Mean predicted (bps) |\n|---|---|---|---|",
        cal.n,
        cal.brier_e8.map_or("- (nothing resolved)".to_string(), |b| b.to_string())
    );
    for b in &cal.bins {
        let mean = if b.n == 0 {
            "-".to_string()
        } else {
            (b.predicted_bps_sum / b.n as u64).to_string()
        };
        let _ = writeln!(
            out,
            "| {}–{} | {} | {} | {mean} |",
            b.lo_bps, b.hi_bps, b.n, b.hits
        );
    }

    out.push_str("\n## 3. Missed / false opportunities\n\nWhich failure class dominates? (from the grades)\n\n| Failure class | Missed | False positive |\n|---|---|---|\n");
    if p.misses.is_empty() {
        out.push_str("| (none graded) | 0 | 0 |\n");
    }
    for (class, t) in &p.misses {
        let _ = writeln!(
            out,
            "| {} | {} | {} |",
            name(class),
            t.missed,
            t.false_positive
        );
    }

    let o = &p.ops;
    let _ = writeln!(
        out,
        "\n## 4. Cost and operator hours\n\nDoes the system save scarce time?\n\n| Measure | Value |\n|---|---|\n| Stage latency | {} ms |\n| Tokens (prompt · completion) | {} · {}{} |\n| Cost | {} |\n| Failed stages | {} |\n| Source failures | {} |\n| Operator correction | {} min |\n| Operator research | {} h |",
        o.latency_ms,
        o.prompt_tokens,
        o.completion_tokens,
        if o.tokens_unknown_stages == 0 {
            String::new()
        } else {
            format!(
                " + UNKNOWN ({} stage(s) ended with no reply)",
                o.tokens_unknown_stages
            )
        },
        o.cost.as_ref().map_or(
            "UNKNOWN (a cycle without token prices, a stage's tokens unknown, or currencies differ)"
                .to_string(),
            |(m, c)| format!("{m} {c}")
        ),
        o.failed_stages,
        o.source_failures,
        o.correction_minutes,
        o.research_hours
    );

    out.push_str("\n## 5. Source coverage and rights\n\nIs expansion justified and lawful?\n\n| Source | Cycles | Terms stated |\n|---|---|---|\n");
    if p.sources.is_empty() {
        out.push_str("| (no source in any packet) | 0 | - |\n");
    }
    for s in &p.sources {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} |",
            s.source_id,
            s.cycles,
            if s.terms_stated { "yes" } else { "NO" }
        );
    }

    out.push_str("\n## 6. Best candidate lane\n\nO6A service / automation / integration vs O6B acquisition / partnership — a tally, not a choice.\n\n| Lane | Ranked | Tests | Held | Rejected |\n|---|---|---|---|---|\n");
    for lane in [Lane::O6A, Lane::O6B] {
        let r = p.lanes.get(&lane).cloned().unwrap_or_default();
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            name(&lane),
            r.ranked,
            r.tests,
            r.held,
            r.rejected
        );
    }

    out.push_str("\n## Replays\n\n| Run | Set | Cases | Development agreement | Holdout read |\n|---|---|---|---|---|\n");
    let replays = json["replays"].as_array().cloned().unwrap_or_default();
    if replays.is_empty() {
        out.push_str("| (none) | - | - | - | - |\n");
    }
    for r in &replays {
        let d = &r["development"];
        let _ = writeln!(
            out,
            "| `{}` | `{}` | {} | {} of {} | {} |",
            r["run"].as_str().unwrap_or_default(),
            r["set"].as_str().unwrap_or_default(),
            r["cases"].as_array().map_or(0, Vec::len),
            d["agree"],
            d["cases"],
            r["holdout_read_line"]
                .as_u64()
                .map_or("not read".to_string(), |n| format!(
                    "holdout-reads.jsonl line {n}"
                ))
        );
    }

    out.push_str("\n## Integrity\n\n| Check | Result |\n|---|---|\n");
    let bad_runs: Vec<String> = check
        .runs
        .iter()
        .filter(|r| !r.ok())
        .map(|r| format!("`{}`", r.run))
        .collect();
    let _ = writeln!(
        out,
        "| Run dirs | {} verified · {} |",
        check.runs.len(),
        if bad_runs.is_empty() {
            "every one MATCH".to_string()
        } else {
            format!("NOT MATCHING: {}", bad_runs.join(", "))
        }
    );
    let _ = writeln!(
        out,
        "| Forecast chain | {} line(s) · {} |",
        check.log_lines,
        if check.chain.is_empty() {
            "intact".to_string()
        } else {
            cell(&check.chain.join("; "))
        }
    );
    for (cycle, state) in check.log.iter().filter(|(_, s)| *s != LogState::Match) {
        let _ = writeln!(out, "| Log line of `{cycle}` | {} |", name(state));
    }
    for s in stale {
        let _ = writeln!(out, "| Resolution | {} |", cell(s));
    }
    for o in &check.open {
        let _ = writeln!(out, "| Open run `{o}` | claimed, never frozen |");
    }

    out.push_str("\n## Stop flags (roadmap § 14)\n\n");
    if p.stop_flags.is_empty() {
        out.push_str("None raised.\n");
    } else {
        out.push_str("| Flag | Detail | Response |\n|---|---|---|\n");
        for f in &p.stop_flags {
            let _ = writeln!(
                out,
                "| {} | {} | {} |",
                name(&f.code),
                cell(&f.detail),
                f.response
            );
        }
    }
    let _ = writeln!(
        out,
        "\n## Decision\n\nThe operator chooses one: {}.\n\n{STOP_LINE}",
        p.decisions.join(" · ")
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::soe::cycle::Target;
    use crate::application::soe::grade::grade_cycle;
    use crate::application::soe::tests::{load_case, Bench};

    fn grade_text(cycle: &str, changed: bool) -> String {
        format!(
            r#"schema = "soe.cycle_grade/1"
id = "g-{cycle}"
version = 1
cycle_id = "{cycle}"
graded_by = "operator"
graded_at = "2026-10-20T09:00:00Z"
relevance = 3
evidence = 4
economics = 3
hidden_labor = 2
novelty_bias = 4
actionability = 2
correction_minutes = 20
research_hours = 1
changed_decision = {changed}

[[misses]]
kind = "MISSED"
candidate = "a-missed-one"
class = "SOURCE_GAP"
"#
        )
    }

    /// The packet has the six roadmap sections in order, the integrity
    /// section, the stop flags, the three choices and the STOP line last;
    /// it is written once under `reviews/<day>/`, frozen and logged.
    #[tokio::test]
    async fn packet_has_all_six_sections() {
        let bench = Bench::new();
        bench
            .run_case(&load_case("weekly_rerun_w42"), Target::Cycle)
            .await;
        grade_cycle(&*bench.store, &grade_text("2026-W41", true))
            .unwrap()
            .unwrap();
        let out = build_review(&*bench.store, &bench.clock, ReviewRange::default()).unwrap();
        let md = &out.markdown;
        let headings = [
            "## 1. Weekly portfolios and grades",
            "## 2. Forecast vs later evidence",
            "## 3. Missed / false opportunities",
            "## 4. Cost and operator hours",
            "## 5. Source coverage and rights",
            "## 6. Best candidate lane",
            "## Replays",
            "## Integrity",
            "## Stop flags (roadmap § 14)",
            "## Decision",
        ];
        let at: Vec<usize> = headings
            .iter()
            .map(|h| md.find(h).unwrap_or_else(|| panic!("{h} missing:\n{md}")))
            .collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "out of order:\n{md}");
        assert!(md.trim_end().ends_with(STOP_LINE), "{md}");
        assert!(md.contains("STOP · REWORK · AUTHORIZE_O5"));
        assert!(out.integrity_ok, "{md}");
        // The JSON carries every part.
        let j = &out.json;
        assert_eq!(j["schema"], REVIEW_SCHEMA);
        assert_eq!(j["cycles"], json!(["2026-W41", "2026-W42"]));
        for part in [
            "cycles",
            "calibration",
            "misses",
            "ops",
            "sources",
            "lanes",
            "stop_flags",
            "decisions",
        ] {
            assert!(!j["packet"][part].is_null(), "packet.{part}");
        }
        assert_eq!(j["packet"]["graded"], 1);
        assert_eq!(j["packet"]["misses"]["SOURCE_GAP"]["missed"], 1);
        assert_eq!(j["stop"], STOP_LINE);
        // The test sources state their terms.
        assert!(md.contains("| Terms stated |"));
        assert!(!j["packet"]["sources"].as_array().unwrap().is_empty());
        // Written once, frozen, logged; a second review the same day is `.2`.
        assert_eq!(out.dir.to_string(), "reviews/2026-10-03");
        assert_eq!(bench.store.status(&out.dir).unwrap(), RunStatus::Frozen);
        assert_eq!(out.line, 1);
        let again = build_review(&*bench.store, &bench.clock, ReviewRange::default()).unwrap();
        assert_eq!(again.dir.to_string(), "reviews/2026-10-03.2");
        // The range keeps one week.
        let one = build_review(
            &*bench.store,
            &bench.clock,
            ReviewRange {
                from: Some("2026-W42".parse().unwrap()),
                to: None,
            },
        )
        .unwrap();
        assert_eq!(one.json["cycles"], json!(["2026-W42"]));
    }

    /// A changed frozen file is an integrity finding in the packet, never
    /// hidden; a cycle claimed and never frozen is listed, not reviewed.
    #[tokio::test]
    async fn integrity_findings_are_shown() {
        let bench = Bench::new();
        bench
            .run_case(&load_case("strong_news_weak_demand"), Target::Cycle)
            .await;
        bench
            .store
            .claim(&RunDir::Cycle("2026-W42".into()))
            .unwrap();
        let dir = RunDir::Cycle("2026-W41".into());
        let f = bench.store.read(&dir, "forecast.json").unwrap().unwrap();
        let edited = String::from_utf8(f)
            .unwrap()
            .replace("\"probability\":3000", "\"probability\":9000");
        bench.store.tamper(&dir, "forecast.json", edited.as_bytes());
        let out = build_review(&*bench.store, &bench.clock, ReviewRange::default()).unwrap();
        assert!(!out.integrity_ok);
        assert!(out.markdown.contains("NOT INTACT"), "{}", out.markdown);
        assert!(out.markdown.contains("NOT MATCHING: `cycles/2026-W41`"));
        assert!(out
            .markdown
            .contains("| Log line of `2026-W41` | MISMATCH |"));
        assert!(out.markdown.contains("Open run `cycles/2026-W42`"));
        assert_eq!(out.json["unfinished"], json!(["2026-W42"]));
        assert_eq!(out.json["cycles"], json!(["2026-W41"]));
    }
}
