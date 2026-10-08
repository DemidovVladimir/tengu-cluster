//! Operator Review #2 packet (roadmap O4 "Operator Review #2 packet", § 14
//! stop conditions, § 15; PRD § 11): what the shadow weeks showed, built
//! from the cycles' records and the operator's grades. Pure. The packet
//! enables a decision; it never makes one — the operator chooses `STOP`,
//! `REWORK` or authorises O5 (out of scope here: a hard stop).
//!
//! | `soe.cycle_grade/1` (the operator's, one per cycle) | Value |
//! |---|---|
//! | header | `schema`, `id`, `version` ≥ 1 |
//! | `cycle_id`, `graded_by`, `graded_at` | the cycle · who · when (known) |
//! | `relevance` `evidence` `economics` `hidden_labor` `novelty_bias` `actionability` | 1..=5 each |
//! | `correction_minutes`, `research_hours` | the operator's time on the cycle |
//! | `changed_decision` | the cycle changed (or would have changed) an allocation decision |
//! | `[[misses]]` | [`Miss`]: `kind` (`MISSED` `FALSE_POSITIVE`), `candidate`, `class` ([`FailureClass`]) |
//! | `note?` | text |
//!
//! | Packet part ([`ReviewPacket::of`]) | Decision it enables |
//! |---|---|
//! | `cycles` (portfolio counts, tests, allocation, unsupported claims, grade) | is the output consistently useful? |
//! | `calibration` (resolved forecast items, `forecast::calibration`) | is confidence calibrated? |
//! | `misses` per [`FailureClass`] | which failure class dominates? |
//! | `ops` (latency, tokens, cost — unknown when any cycle's is — failed stages, source failures, correction minutes, research hours) | does it save scarce time? |
//! | `sources` (each source the cycles cited, its terms stated or not) | is expansion justified and lawful? |
//! | `lanes` (`O6A` service / integration · `O6B` acquisition / partnership, by mechanism) | which lane, if any? |
//! | `stop_flags` · `enough_cycles` | § 14 conditions met · ≥ [`MIN_LIVE_CYCLES`] cycles |
//!
//! | Stop flag ([`stop_flags`]) | When | Response (roadmap § 14) |
//! |---|---|---|
//! | `CORRECTION_BURDEN_HIGH` | ≥ 2 graded cycles; the latest's correction minutes > 0 and not below the first graded one's | do not add sources or automation |
//! | `REPEATED_UNSUPPORTED_ECONOMICS` | ≥ 2 cycles with an unsupported claim (a known input without basis) | stop live validation; return to O1/O3 evaluation |
//! | `SOURCE_TERMS_UNCLEAR` | a cited source without terms text or a terms sha256 | disable the source and derived public output; keep only permitted metadata |
//! | `NO_DECISION_VALUE` | ≥ [`MIN_LIVE_CYCLES`] graded cycles, none `changed_decision` | stop or narrow scope; do not advance because infrastructure exists |
//! | `ACQUISITION_DOWNSIDE_UNBOUNDED` | the latest cycle holds or rejects an `ACQUIRE_TRANSFORM` candidate on `DOWNSIDE_UNSTATED` | NO-GO; never compensate with a higher model confidence |
//!
//! O4 conditions outside a packet (weekend evidence, approval bypass,
//! realised low-margin work) are O5+ or operational — not flagged here.

// Consumers land with `tengu soe review` (O4).
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::forecast::{calibration, Calibration, Resolved};
use super::opportunity::Mechanism;
use super::ops::{Cost, CycleOps};
use super::portfolio::{IsoWeek, PortfolioAction, WeeklyPortfolio};
use super::record::{Problems, SoeRecord};
use super::value::{codes, Bps, Currency, Minor, SchemaTag};
use crate::domain::evidence::valid_sha256;
use crate::domain::lineage::value::Time;

/// Live weekly cycles the review needs (roadmap O4 "4 complete weekly cycles").
pub const MIN_LIVE_CYCLES: usize = 4;
/// Calibration bins of the packet.
const CAL_BINS: u16 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MissKind {
    /// A good opportunity the cycle did not surface or ranked out.
    Missed,
    /// A candidate the cycle favoured that later proved poor.
    FalsePositive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FailureClass {
    Evidence,
    Economics,
    HiddenLabor,
    NoveltyBias,
    Capability,
    LegalAccess,
    SourceGap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Miss {
    pub kind: MissKind,
    pub candidate: String,
    pub class: FailureClass,
}

/// `soe.cycle_grade/1` (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CycleGrade {
    pub schema: SchemaTag,
    pub id: String,
    pub version: u32,
    pub cycle_id: String,
    pub graded_by: String,
    pub graded_at: Time,
    pub relevance: u8,
    pub evidence: u8,
    pub economics: u8,
    pub hidden_labor: u8,
    pub novelty_bias: u8,
    pub actionability: u8,
    pub correction_minutes: u32,
    pub research_hours: u32,
    pub changed_decision: bool,
    pub misses: Vec<Miss>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl CycleGrade {
    /// The six 1..=5 grades, by name.
    pub fn scores(&self) -> [(&'static str, u8); 6] {
        [
            ("relevance", self.relevance),
            ("evidence", self.evidence),
            ("economics", self.economics),
            ("hidden_labor", self.hidden_labor),
            ("novelty_bias", self.novelty_bias),
            ("actionability", self.actionability),
        ]
    }
}

impl SoeRecord for CycleGrade {
    const RECORD: &'static str = "cycle_grade";

    fn schema(&self) -> &SchemaTag {
        &self.schema
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn problems(&self, p: &mut Problems) {
        p.id("cycle_id", &self.cycle_id);
        p.text("graded_by", &self.graded_by);
        p.known("graded_at", &self.graded_at);
        for (f, v) in self.scores() {
            p.check(
                (1..=5).contains(&v),
                codes::INVALID_FIELD,
                f,
                format_args!("{v}: a grade is 1..=5"),
            );
        }
        for (i, m) in self.misses.iter().enumerate() {
            p.text(&format!("misses[{i}].candidate"), &m.candidate);
        }
    }
}

/// The terms a cited source was read under (from the packets' citations).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTerms {
    pub source_id: String,
    pub license_or_terms: String,
    pub terms_sha256: String,
}

impl SourceTerms {
    pub fn stated(&self) -> bool {
        !self.license_or_terms.trim().is_empty() && valid_sha256(&self.terms_sha256)
    }
}

/// One cycle as the review reads it.
#[derive(Debug, Clone, Copy)]
pub struct CycleInput<'a> {
    pub cycle_id: &'a str,
    pub portfolio: &'a WeeklyPortfolio,
    /// Candidate id → its mechanism.
    pub mechanisms: &'a BTreeMap<String, Mechanism>,
    /// Known inputs without a basis, over the cycle's proposals.
    pub unsupported_claims: usize,
    pub sources: &'a [SourceTerms],
    pub ops: &'a CycleOps,
    pub grade: Option<&'a CycleGrade>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StopCode {
    CorrectionBurdenHigh,
    RepeatedUnsupportedEconomics,
    SourceTermsUnclear,
    NoDecisionValue,
    AcquisitionDownsideUnbounded,
}

impl StopCode {
    /// Roadmap § 14's response.
    pub fn response(self) -> &'static str {
        match self {
            StopCode::CorrectionBurdenHigh => "do not add sources or automation",
            StopCode::RepeatedUnsupportedEconomics => {
                "stop live validation; return to O1/O3 evaluation"
            }
            StopCode::SourceTermsUnclear => {
                "disable the source and derived public output; keep only permitted metadata"
            }
            StopCode::NoDecisionValue => {
                "stop or narrow scope; do not advance because infrastructure exists"
            }
            StopCode::AcquisitionDownsideUnbounded => {
                "NO-GO; never compensate with a higher model confidence"
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StopFlag {
    pub code: StopCode,
    pub response: &'static str,
    pub detail: String,
}

fn flag(code: StopCode, detail: String) -> StopFlag {
    StopFlag {
        code,
        response: code.response(),
        detail,
    }
}

/// Module table: the § 14 conditions `cycles` (oldest first) meet.
pub fn stop_flags(cycles: &[CycleInput]) -> Vec<StopFlag> {
    let mut out = Vec::new();
    let graded: Vec<&CycleGrade> = cycles.iter().filter_map(|c| c.grade).collect();
    if let (Some(first), Some(last)) = (graded.first(), graded.last()) {
        if graded.len() >= 2
            && last.correction_minutes > 0
            && last.correction_minutes >= first.correction_minutes
        {
            out.push(flag(
                StopCode::CorrectionBurdenHigh,
                format!(
                    "correction minutes {} → {} over {} graded cycles",
                    first.correction_minutes,
                    last.correction_minutes,
                    graded.len()
                ),
            ));
        }
    }
    let unsupported: Vec<&str> = cycles
        .iter()
        .filter(|c| c.unsupported_claims > 0)
        .map(|c| c.cycle_id)
        .collect();
    if unsupported.len() >= 2 {
        out.push(flag(
            StopCode::RepeatedUnsupportedEconomics,
            format!("unsupported claims in {}", unsupported.join(", ")),
        ));
    }
    let unclear: BTreeSet<&str> = cycles
        .iter()
        .flat_map(|c| c.sources)
        .filter(|s| !s.stated())
        .map(|s| s.source_id.as_str())
        .collect();
    if !unclear.is_empty() {
        out.push(flag(
            StopCode::SourceTermsUnclear,
            format!(
                "no terms text or sha256: {}",
                unclear.into_iter().collect::<Vec<_>>().join(", ")
            ),
        ));
    }
    if graded.len() >= MIN_LIVE_CYCLES && !graded.iter().any(|g| g.changed_decision) {
        out.push(flag(
            StopCode::NoDecisionValue,
            format!("{} graded cycles, none changed a decision", graded.len()),
        ));
    }
    if let Some(last) = cycles.last() {
        let unbounded: Vec<&str> = last
            .portfolio
            .held
            .iter()
            .chain(&last.portfolio.rejected)
            .filter(|r| {
                last.mechanisms.get(&r.id) == Some(&Mechanism::AcquireTransform)
                    && r.gates.iter().any(|g| g == "DOWNSIDE_UNSTATED")
            })
            .map(|r| r.id.as_str())
            .collect();
        if !unbounded.is_empty() {
            out.push(flag(
                StopCode::AcquisitionDownsideUnbounded,
                format!(
                    "{}: downside not stated in {}",
                    unbounded.join(", "),
                    last.cycle_id
                ),
            ));
        }
    }
    out
}

/// One cycle's row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CycleRow {
    pub cycle_id: String,
    pub week: IsoWeek,
    pub hold: bool,
    pub ranked: usize,
    pub held: usize,
    pub rejected: usize,
    /// `CHEAP_TEST` and `CONTINUE_ACTIVE` actions.
    pub tests: usize,
    pub owner_hours: u32,
    pub cash: Minor,
    pub unsupported_claims: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grade: Option<BTreeMap<&'static str, u8>>,
    pub changed_decision: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Lane {
    /// Service, automation, integration, productized delivery.
    O6A,
    /// Existing assets, partnership / revenue share.
    O6B,
}

/// The lane a mechanism belongs to (`HOLD` / `REJECT`: none).
pub fn lane_of(m: Mechanism) -> Option<Lane> {
    match m {
        Mechanism::AcquireTransform | Mechanism::PartnerRevshare => Some(Lane::O6B),
        Mechanism::Hold | Mechanism::Reject => None,
        _ => Some(Lane::O6A),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LaneRow {
    pub ranked: usize,
    pub tests: usize,
    pub held: usize,
    pub rejected: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MissTally {
    pub missed: usize,
    pub false_positive: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpsTotals {
    pub latency_ms: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub failed_stages: usize,
    pub source_failures: usize,
    /// Σ cycle costs when every one is known in one currency; else none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<(Minor, Currency)>,
    pub correction_minutes: u32,
    pub research_hours: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceRow {
    pub source_id: String,
    pub terms_stated: bool,
    pub cycles: usize,
}

/// Module table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewPacket {
    pub cycles: Vec<CycleRow>,
    pub enough_cycles: bool,
    pub graded: usize,
    pub calibration: Calibration,
    pub misses: BTreeMap<FailureClass, MissTally>,
    pub ops: OpsTotals,
    pub sources: Vec<SourceRow>,
    pub lanes: BTreeMap<Lane, LaneRow>,
    pub stop_flags: Vec<StopFlag>,
    /// What the operator chooses between.
    pub decisions: [&'static str; 3],
}

fn is_test(a: &PortfolioAction) -> bool {
    matches!(
        a,
        PortfolioAction::CheapTest { .. } | PortfolioAction::ContinueActive
    )
}

impl ReviewPacket {
    /// Module table: the packet of `cycles` (oldest first) and the forecast
    /// items resolved so far.
    pub fn of(cycles: &[CycleInput], resolved: &[Resolved]) -> ReviewPacket {
        let mut lanes: BTreeMap<Lane, LaneRow> = BTreeMap::new();
        let mut misses: BTreeMap<FailureClass, MissTally> = BTreeMap::new();
        let mut sources: BTreeMap<&str, (bool, usize)> = BTreeMap::new();
        let mut ops = OpsTotals {
            latency_ms: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            failed_stages: 0,
            source_failures: 0,
            cost: None,
            correction_minutes: 0,
            research_hours: 0,
        };
        let mut cost: Option<Option<(Minor, Currency)>> = None;
        let mut rows = Vec::new();
        for c in cycles {
            let p = c.portfolio;
            let lane = |id: &str| c.mechanisms.get(id).and_then(|m| lane_of(*m));
            for r in &p.ranked {
                if let Some(l) = lane(&r.id) {
                    let e = lanes.entry(l).or_default();
                    e.ranked += 1;
                    e.tests += usize::from(is_test(&r.action));
                }
            }
            for r in &p.held {
                if let Some(l) = lane(&r.id) {
                    let e = lanes.entry(l).or_default();
                    e.held += 1;
                    e.tests += usize::from(is_test(&r.action));
                }
            }
            for r in &p.rejected {
                if let Some(l) = lane(&r.id) {
                    lanes.entry(l).or_default().rejected += 1;
                }
            }
            let mut seen = BTreeSet::new();
            for s in c.sources {
                if seen.insert(s.source_id.as_str()) {
                    let e = sources.entry(&s.source_id).or_insert((true, 0));
                    e.0 &= s.stated();
                    e.1 += 1;
                }
            }
            let o = c.ops;
            ops.latency_ms = ops.latency_ms.saturating_add(o.latency_ms);
            ops.prompt_tokens = ops.prompt_tokens.saturating_add(o.prompt_tokens);
            ops.completion_tokens = ops.completion_tokens.saturating_add(o.completion_tokens);
            ops.failed_stages += o.failed_stages;
            ops.source_failures += o.source_failures.len();
            cost = Some(match (cost, &o.cost) {
                (None, Cost::Known { amount, currency }) => Some((*amount, *currency)),
                (Some(Some((sum, cur))), Cost::Known { amount, currency }) if cur == *currency => {
                    sum.checked_add(*amount).ok().map(|s| (s, cur))
                }
                _ => None,
            });
            if let Some(g) = c.grade {
                ops.correction_minutes =
                    ops.correction_minutes.saturating_add(g.correction_minutes);
                ops.research_hours = ops.research_hours.saturating_add(g.research_hours);
                for m in &g.misses {
                    let e = misses.entry(m.class).or_default();
                    match m.kind {
                        MissKind::Missed => e.missed += 1,
                        MissKind::FalsePositive => e.false_positive += 1,
                    }
                }
            }
            rows.push(CycleRow {
                cycle_id: c.cycle_id.to_string(),
                week: p.week,
                hold: p.is_hold(),
                ranked: p.ranked.len(),
                held: p.held.len(),
                rejected: p.rejected.len(),
                tests: p
                    .ranked
                    .iter()
                    .map(|r| &r.action)
                    .chain(p.held.iter().map(|r| &r.action))
                    .filter(|a| is_test(a))
                    .count(),
                owner_hours: p.allocation.owner_hours,
                cash: p.allocation.cash,
                unsupported_claims: c.unsupported_claims,
                grade: c.grade.map(|g| g.scores().into_iter().collect()),
                changed_decision: c.grade.map(|g| g.changed_decision),
            });
        }
        ops.cost = cost.flatten();
        let points: Vec<(Bps, bool)> = resolved.iter().map(|r| (r.probability, r.hit)).collect();
        ReviewPacket {
            enough_cycles: cycles.len() >= MIN_LIVE_CYCLES,
            graded: cycles.iter().filter(|c| c.grade.is_some()).count(),
            cycles: rows,
            calibration: calibration(&points, CAL_BINS),
            misses,
            ops,
            sources: sources
                .into_iter()
                .map(|(id, (terms_stated, cycles))| SourceRow {
                    source_id: id.to_string(),
                    terms_stated,
                    cycles,
                })
                .collect(),
            lanes,
            stop_flags: stop_flags(cycles),
            decisions: ["STOP", "REWORK", "AUTHORIZE_O5"],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::soe::allocate::tests::{automation, week_of};
    use crate::domain::soe::allocate::Track;
    use crate::domain::soe::economics::tests::VERIFIED_DEAL;
    use crate::domain::soe::gates::tests::acquisition;
    use crate::domain::soe::ops::tests::stages;
    use crate::domain::soe::ops::{cycle_ops, TokenPrices};
    use crate::domain::soe::record::validate;

    const H: &str = "0000000000000000000000000000000000000000000000000000000000000004";

    fn grade(cycle: &str, corrections: u32, changed: bool) -> CycleGrade {
        CycleGrade {
            schema: SchemaTag::v1("cycle_grade").unwrap(),
            id: format!("grade-{cycle}"),
            version: 1,
            cycle_id: cycle.into(),
            graded_by: "operator".into(),
            graded_at: "2026-10-06T08:00:00Z".parse().unwrap(),
            relevance: 3,
            evidence: 4,
            economics: 3,
            hidden_labor: 2,
            novelty_bias: 4,
            actionability: 2,
            correction_minutes: corrections,
            research_hours: 2,
            changed_decision: changed,
            misses: vec![],
            note: None,
        }
    }

    fn terms(id: &str, stated: bool) -> SourceTerms {
        SourceTerms {
            source_id: id.into(),
            license_or_terms: if stated {
                "public domain (US gov)".into()
            } else {
                String::new()
            },
            terms_sha256: H.into(),
        }
    }

    /// `n` cycles over the same week, graded with `corrections[i]`.
    fn run(
        portfolio: &WeeklyPortfolio,
        mechanisms: &BTreeMap<String, Mechanism>,
        ops: &CycleOps,
        sources: &[SourceTerms],
        grades: &[CycleGrade],
        unsupported: &[usize],
    ) -> Vec<StopFlag> {
        let ids: Vec<String> = grades.iter().map(|g| g.cycle_id.clone()).collect();
        let cycles: Vec<CycleInput> = grades
            .iter()
            .zip(&ids)
            .zip(unsupported)
            .map(|((g, id), u)| CycleInput {
                cycle_id: id,
                portfolio,
                mechanisms,
                unsupported_claims: *u,
                sources,
                ops,
                grade: Some(g),
            })
            .collect();
        stop_flags(&cycles)
    }

    fn codes(flags: &[StopFlag]) -> Vec<StopCode> {
        flags.iter().map(|f| f.code).collect()
    }

    #[test]
    fn stop_flags_from_grades() {
        let a = automation("a", "\"600.00\"");
        let w = week_of(&[(&a, Track::New)]).portfolio;
        let mech = BTreeMap::from([("a".to_string(), a.mechanism)]);
        let ops = cycle_ops("c", stages(), vec![], None).unwrap();
        let ok = [terms("sec_edgar", true)];
        // Four weeks: corrections rise, nothing changed a decision, two
        // weeks with an unsupported claim.
        let bad: Vec<CycleGrade> = [30, 35, 40, 45]
            .iter()
            .enumerate()
            .map(|(i, m)| grade(&format!("2026-W4{}", i + 1), *m, false))
            .collect();
        let flags = run(&w, &mech, &ops, &ok, &bad, &[1, 0, 2, 0]);
        assert_eq!(
            codes(&flags),
            [
                StopCode::CorrectionBurdenHigh,
                StopCode::RepeatedUnsupportedEconomics,
                StopCode::NoDecisionValue
            ]
        );
        assert_eq!(
            flags[0].detail,
            "correction minutes 30 → 45 over 4 graded cycles"
        );
        assert_eq!(flags[2].response, StopCode::NoDecisionValue.response());
        // Improving corrections, one decision changed, every claim supported.
        let good: Vec<CycleGrade> = [40, 30, 20, 10]
            .iter()
            .enumerate()
            .map(|(i, m)| grade(&format!("2026-W4{}", i + 1), *m, i == 2))
            .collect();
        assert!(run(&w, &mech, &ops, &ok, &good, &[0, 0, 1, 0]).is_empty());
        // Three weeks without value are not yet four.
        assert!(!codes(&run(&w, &mech, &ops, &ok, &bad[..3], &[0, 0, 0]))
            .contains(&StopCode::NoDecisionValue));
        // A source read without stated terms.
        let unclear = [terms("sec_edgar", true), terms("ted_search", false)];
        let f = run(&w, &mech, &ops, &unclear, &good, &[0, 0, 0, 0]);
        assert_eq!(codes(&f), [StopCode::SourceTermsUnclear]);
        assert_eq!(f[0].detail, "no terms text or sha256: ted_search");
        // An acquisition whose downside is unstated in the latest week.
        let mut acq = acquisition(VERIFIED_DEAL);
        acq.risk.max_loss.dependency = "UNKNOWN".into();
        let wa = week_of(&[(&acq, Track::New)]).portfolio;
        assert!(wa.held[0].gates.contains(&"DOWNSIDE_UNSTATED".to_string()));
        let mech_a = BTreeMap::from([(acq.id.clone(), acq.mechanism)]);
        let f = run(&wa, &mech_a, &ops, &ok, &good[..1], &[0]);
        assert_eq!(codes(&f), [StopCode::AcquisitionDownsideUnbounded]);
        // Grades are 1..=5.
        let mut g = grade("2026-W41", 0, false);
        g.actionability = 6;
        assert_eq!(validate(&g).unwrap_err()[0].code, codes::INVALID_FIELD);
    }

    #[test]
    fn packet_tallies_lanes_ops_misses_and_calibration() {
        let a = automation("a", "\"600.00\"");
        let mut acq = acquisition(VERIFIED_DEAL);
        acq.id = "acq".into();
        let w = week_of(&[(&a, Track::New), (&acq, Track::New)]).portfolio;
        let mech = BTreeMap::from([
            ("a".to_string(), a.mechanism),
            ("acq".to_string(), acq.mechanism),
        ]);
        let prices = TokenPrices {
            currency: Currency::Usd,
            prompt_per_million: "3.00".parse().unwrap(),
            completion_per_million: "15.00".parse().unwrap(),
        };
        let priced = cycle_ops("c1", stages(), vec!["ted_search".into()], Some(&prices)).unwrap();
        let unpriced = cycle_ops("c2", stages(), vec![], None).unwrap();
        let mut g1 = grade("2026-W41", 20, true);
        g1.misses = vec![
            Miss {
                kind: MissKind::FalsePositive,
                candidate: "a".into(),
                class: FailureClass::HiddenLabor,
            },
            Miss {
                kind: MissKind::Missed,
                candidate: "elsewhere".into(),
                class: FailureClass::SourceGap,
            },
        ];
        let src = [terms("sec_edgar", true), terms("sec_edgar", true)];
        let c1 = CycleInput {
            cycle_id: "2026-W41",
            portfolio: &w,
            mechanisms: &mech,
            unsupported_claims: 0,
            sources: &src,
            ops: &priced,
            grade: Some(&g1),
        };
        let p = ReviewPacket::of(&[c1], &[]);
        assert_eq!(p.cycles.len(), 1);
        assert!(!p.enough_cycles);
        assert_eq!(p.cycles[0].tests, 2);
        assert_eq!(p.lanes[&Lane::O6A].ranked, 1);
        assert_eq!(p.lanes[&Lane::O6B].ranked, 1);
        assert_eq!(p.misses[&FailureClass::HiddenLabor].false_positive, 1);
        assert_eq!(p.misses[&FailureClass::SourceGap].missed, 1);
        assert_eq!(p.ops.cost, Some(("0.17".parse().unwrap(), Currency::Usd)));
        assert_eq!((p.ops.source_failures, p.ops.correction_minutes), (1, 20));
        assert_eq!(
            p.sources,
            [SourceRow {
                source_id: "sec_edgar".into(),
                terms_stated: true,
                cycles: 1
            }]
        );
        assert_eq!(p.decisions, ["STOP", "REWORK", "AUTHORIZE_O5"]);
        // One cycle without prices: the total cost is unknown, never a sum of part.
        let c2 = CycleInput {
            cycle_id: "2026-W42",
            ops: &unpriced,
            grade: None,
            ..c1
        };
        let p = ReviewPacket::of(&[c1, c2], &[]);
        assert_eq!(p.ops.cost, None);
        assert_eq!(p.graded, 1);
        // Calibration from resolved items.
        let r = |bps: i64, hit: bool| Resolved {
            candidate: "a".into(),
            item: 0,
            probability: Bps::new(bps).unwrap(),
            hit,
        };
        let p = ReviewPacket::of(&[c1], &[r(8000, true), r(2000, false)]);
        assert_eq!(p.calibration.n, 2);
        // (0.2² + 0.2²) / 2 = 0.04.
        assert_eq!(p.calibration.brier_e8, Some(4_000_000));
    }
}
