//! O0 evaluation set (roadmap O0 "≥ 12 dated cases"; `docs/soe-2026-10-08.md`
//! § 7): one case = one dated decision — the views of the source records,
//! the candidates, and what the gates and the ranking must answer — replayed
//! against the profile it was written for. Pure; the cases are
//! `tests/fixtures/soe/cases/<id>.toml`, read by `config/soe.rs`
//! (`load_record_dir`).
//!
//! | `soe.eval_case/1` | Value |
//! |---|---|
//! | header | `schema`, `id` (= the file stem), `version` ≥ 1 |
//! | `as_of` | the decision time (known) |
//! | `class` | [`CaseClass`]: `GOOD` `BAD` `STALE` `CONTRADICTORY` `HOLD` `BOUNDARY` `LOOKAHEAD` |
//! | `synthetic` | `true`: invented example values and `synthetic:` record ids — no real firm, person or figure |
//! | `profile` | the profile id the expectations hold for (a boundary is the profile's value) |
//! | `note` | what the case tests |
//! | `[[cited]]` | `gates::CitedRecord` views (record ids unique) |
//! | `[[candidates]]` | `soe.opportunity/1` records, each valid; an (id, version) once |
//! | `[expected]` | `verdicts` (one per candidate id) · `gates` (labels, any order; optional per id) · `ranked` (exactly the `PASS` ids, in rank order) · `next_information` (in order) · `versions` (optional) |
//!
//! | [`run_case`] check | A diff when |
//! |---|---|
//! | the answer | a verdict, gate set, version, the ranking or the next information differs from `[expected]` |
//! | no look-ahead | dropping every record not knowable at `as_of` (and every later contradiction) changes a verdict or a rank key |
//! | `HOLD` week | nothing ranks and `rank::hold_week` refuses the week |
//!
//! A case run under another profile is refused (`profile_mismatch`).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::gates::{cited_problems, CitedRecord};
use super::opportunity::Opportunity;
use super::portfolio::IsoWeek;
use super::profile::OperatorProfile;
use super::rank::{
    assess, current_versions, hold_week, rank, week_next_information, Assessment, WeekHead,
};
use super::record::{validate, Problems, SoeRecord, Verdict};
use super::value::{codes, SchemaTag, ValueError};
use crate::domain::canonical::canonical_sha256;
use crate::domain::lineage::value::{Time, TimeOrder};

/// What a case exercises (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CaseClass {
    Good,
    Bad,
    Stale,
    Contradictory,
    Hold,
    Boundary,
    Lookahead,
}

impl CaseClass {
    pub const ALL: [CaseClass; 7] = [
        CaseClass::Good,
        CaseClass::Bad,
        CaseClass::Stale,
        CaseClass::Contradictory,
        CaseClass::Hold,
        CaseClass::Boundary,
        CaseClass::Lookahead,
    ];
}

/// `[expected]` (module table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    pub verdicts: BTreeMap<String, Verdict>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub gates: BTreeMap<String, Vec<String>>,
    pub ranked: Vec<String>,
    pub next_information: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub versions: BTreeMap<String, u32>,
}

/// `soe.eval_case/1` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalCase {
    pub schema: SchemaTag,
    pub id: String,
    pub version: u32,
    pub as_of: Time,
    pub class: CaseClass,
    pub synthetic: bool,
    pub profile: String,
    pub note: String,
    #[serde(default)]
    pub cited: Vec<CitedRecord>,
    pub expected: Expected,
    pub candidates: Vec<Opportunity>,
}

impl SoeRecord for EvalCase {
    const RECORD: &'static str = "eval_case";

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
        p.known("as_of", &self.as_of);
        p.id("profile", &self.profile);
        p.text("note", &self.note);
        cited_problems(&self.cited, p);
        p.unique(
            "candidates (id, version)",
            self.candidates.iter().map(|c| (&c.id, c.version)),
        );
        for (i, c) in self.candidates.iter().enumerate() {
            if let Err(errors) = validate(c) {
                for e in errors {
                    p.push(e.code, &format!("candidates[{i}]"), e.message);
                }
            }
        }

        let ids: BTreeSet<&str> = self.candidates.iter().map(|c| c.id.as_str()).collect();
        let x = &self.expected;
        let verdict_ids: BTreeSet<&str> = x.verdicts.keys().map(String::as_str).collect();
        for id in ids.difference(&verdict_ids) {
            p.push(
                codes::INVALID_FIELD,
                "expected.verdicts",
                format_args!("candidate `{id}` has no expected verdict"),
            );
        }
        for (field, keys) in [
            ("expected.verdicts", verdict_ids.clone()),
            (
                "expected.gates",
                x.gates.keys().map(String::as_str).collect(),
            ),
            (
                "expected.versions",
                x.versions.keys().map(String::as_str).collect(),
            ),
        ] {
            for id in keys.difference(&ids) {
                p.push(
                    codes::INVALID_FIELD,
                    field,
                    format_args!("`{id}` is not a candidate"),
                );
            }
        }
        for (id, labels) in &x.gates {
            p.unique_texts(
                &format!("expected.gates.{id}"),
                labels.iter().map(String::as_str),
            );
            p.check(
                labels.is_empty() || x.verdicts.get(id) != Some(&Verdict::Pass),
                codes::INVALID_FIELD,
                &format!("expected.gates.{id}"),
                "a PASS candidate fails no gate",
            );
        }
        for (id, v) in &x.versions {
            p.check(
                self.candidates
                    .iter()
                    .any(|c| &c.id == id && c.version == *v),
                codes::INVALID_FIELD,
                &format!("expected.versions.{id}"),
                format_args!("no candidate `{id}` version {v}"),
            );
        }
        p.unique("expected.ranked", x.ranked.iter());
        let passing: BTreeSet<&str> = x
            .verdicts
            .iter()
            .filter(|(_, v)| **v == Verdict::Pass)
            .map(|(id, _)| id.as_str())
            .collect();
        let ranked: BTreeSet<&str> = x.ranked.iter().map(String::as_str).collect();
        p.check(
            ranked == passing,
            codes::INVALID_FIELD,
            "expected.ranked",
            format_args!("ranks exactly the PASS candidates {passing:?}, not {ranked:?}"),
        );
        p.unique_texts(
            "expected.next_information",
            x.next_information.iter().map(String::as_str),
        );
    }
}

/// What the gates and the ranking answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Answer {
    pub verdicts: BTreeMap<String, Verdict>,
    pub gates: BTreeMap<String, Vec<String>>,
    pub versions: BTreeMap<String, u32>,
    pub ranked: Vec<String>,
    pub next_information: Vec<String>,
}

/// One case's outcome: `ok` = no diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaseResult {
    pub id: String,
    pub class: CaseClass,
    pub ok: bool,
    pub diffs: Vec<String>,
    pub answer: Answer,
}

fn assess_all(
    case: &EvalCase,
    cited: &[CitedRecord],
    profile: &OperatorProfile,
) -> Result<Vec<Assessment>, ValueError> {
    current_versions(&case.candidates, &case.as_of)?
        .into_iter()
        .map(|o| assess(o, cited, profile, case.as_of))
        .collect()
}

fn answer_of(all: &[Assessment], profile: &OperatorProfile) -> Answer {
    Answer {
        verdicts: all
            .iter()
            .map(|a| (a.id.clone(), a.verdict.verdict))
            .collect(),
        gates: all
            .iter()
            .map(|a| (a.id.clone(), a.verdict.labels()))
            .collect(),
        versions: all.iter().map(|a| (a.id.clone(), a.version)).collect(),
        ranked: rank(all, &profile.rank_order)
            .iter()
            .map(|a| a.id.clone())
            .collect(),
        next_information: week_next_information(all),
    }
}

fn name<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

fn diffs(x: &Expected, got: &Answer) -> Vec<String> {
    let mut out = Vec::new();
    for (id, want) in &x.verdicts {
        match got.verdicts.get(id) {
            Some(v) if v == want => {}
            Some(v) => out.push(format!(
                "verdict `{id}`: expected {}, got {}",
                name(want),
                name(v)
            )),
            None => out.push(format!(
                "verdict `{id}`: expected {}, got none (no version on or before as_of)",
                name(want)
            )),
        }
    }
    for (id, want) in &x.gates {
        let want: BTreeSet<&String> = want.iter().collect();
        let have: BTreeSet<&String> = got.gates.get(id).into_iter().flatten().collect();
        if want != have {
            let list = |s: BTreeSet<&&String>| {
                s.into_iter()
                    .map(|l| l.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            out.push(format!(
                "gates `{id}`: missing [{}]; unexpected [{}]",
                list(want.difference(&have).collect()),
                list(have.difference(&want).collect())
            ));
        }
    }
    for (id, want) in &x.versions {
        let have = got.versions.get(id);
        if have != Some(want) {
            out.push(format!(
                "version `{id}`: expected {want}, got {}",
                have.map_or("none".to_string(), u32::to_string)
            ));
        }
    }
    if x.ranked != got.ranked {
        out.push(format!(
            "ranked: expected [{}], got [{}]",
            x.ranked.join(", "),
            got.ranked.join(", ")
        ));
    }
    if x.next_information != got.next_information {
        out.push(format!(
            "next_information: expected [{}], got [{}]",
            x.next_information.join(", "),
            got.next_information.join(", ")
        ));
    }
    out
}

/// Module table: `case` replayed under `profile` (refused under another
/// profile, or when the gates refuse a candidate).
pub fn run_case(case: &EvalCase, profile: &OperatorProfile) -> Result<CaseResult, ValueError> {
    if case.profile != profile.id {
        return Err(ValueError::new(
            codes::PROFILE_MISMATCH,
            format!(
                "case `{}` holds for profile `{}`, not `{}`",
                case.id, case.profile, profile.id
            ),
        ));
    }
    let as_of = case.as_of;
    let all = assess_all(case, &case.cited, profile)?;
    let answer = answer_of(&all, profile);
    let mut out = diffs(&case.expected, &answer);

    // No look-ahead: what was not knowable at `as_of` changes nothing.
    let blind: Vec<CitedRecord> = case
        .cited
        .iter()
        .filter(|c| c.knowable(&as_of))
        .map(|c| {
            let mut c = c.clone();
            if c.contradicted_at
                .is_some_and(|t| t.order(&as_of) == TimeOrder::After)
            {
                c.contradicted_at = None;
            }
            c
        })
        .collect();
    for (a, b) in all.iter().zip(assess_all(case, &blind, profile)?) {
        if a.verdict != b.verdict || a.keys != b.keys {
            out.push(format!(
                "look-ahead: `{}` changes when the records not knowable at {as_of} are dropped",
                a.id
            ));
        }
    }

    // A week where nothing ranks is a valid HOLD week.
    if answer.ranked.is_empty() {
        let head = IsoWeek::of(&as_of).map(|week| WeekHead {
            id: week.to_string(),
            week,
            as_of,
            currency: profile.currency,
            profile_sha256: canonical_sha256(&serde_json::to_value(profile).unwrap_or_default()),
        });
        match head {
            None => out.push(format!("hold week: no ISO week for {as_of}")),
            Some(head) => {
                if let Err(errors) = hold_week(head, &all) {
                    for e in errors {
                        out.push(format!("hold week: {e}"));
                    }
                }
            }
        }
    }
    Ok(CaseResult {
        id: case.id.clone(),
        class: case.class,
        ok: out.is_empty(),
        diffs: out,
        answer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::soe::gates::tests::{at, cited, complete, passing};
    use crate::domain::soe::opportunity::tests::recurring;
    use crate::domain::soe::profile::tests::synthetic;
    use crate::domain::soe::record::from_toml;

    /// A passing revenue share `pass` and the recurring example `held`
    /// (price UNKNOWN), with the given expectations.
    fn case(expected: Expected) -> EvalCase {
        let mut pass = passing();
        pass.id = "pass".into();
        let mut held = complete(recurring());
        held.id = "held".into();
        EvalCase {
            schema: SchemaTag::v1("eval_case").unwrap(),
            id: "probe".into(),
            version: 1,
            as_of: at(),
            class: CaseClass::Good,
            synthetic: true,
            profile: "synthetic-operator".into(),
            note: "one passes, one holds".into(),
            cited: cited(),
            expected,
            candidates: vec![pass, held],
        }
    }

    fn right() -> Expected {
        Expected {
            verdicts: BTreeMap::from([
                ("pass".into(), Verdict::Pass),
                ("held".into(), Verdict::Hold),
            ]),
            gates: BTreeMap::from([(
                "held".into(),
                vec!["UNKNOWN_INPUT:economics.revenue.price_per_month".into()],
            )]),
            ranked: vec!["pass".into()],
            next_information: vec!["economics.revenue.price_per_month".into()],
            versions: BTreeMap::from([("pass".into(), 1)]),
        }
    }

    #[test]
    fn run_case_reports_each_diff() {
        let c = case(right());
        assert!(validate(&c).is_ok());
        let r = run_case(&c, &synthetic()).unwrap();
        assert!(r.ok, "{:?}", r.diffs);
        assert_eq!(r.answer.ranked, ["pass"]);

        // The same case through TOML (nested candidate tables) and back.
        let text = toml::to_string(&c).unwrap();
        assert_eq!(from_toml::<EvalCase>(&text).unwrap(), c);

        // Every expectation wrong: one diff each.
        let mut wrong = right();
        wrong.verdicts.insert("held".into(), Verdict::Reject);
        wrong
            .gates
            .insert("held".into(), vec!["CASH_EXPOSURE_ABOVE_CAP".into()]);
        wrong.versions.insert("pass".into(), 2);
        wrong.next_information = vec![];
        let mut c = case(wrong);
        c.candidates[0].version = 2;
        c.candidates.push({
            let mut v1 = c.candidates[0].clone();
            v1.version = 1;
            v1.as_of = "2026-10-06".parse().unwrap(); // after the decision
            v1
        });
        let r = run_case(&c, &synthetic()).unwrap();
        assert!(!r.ok);
        assert_eq!(
            r.diffs,
            [
                "verdict `held`: expected REJECT, got HOLD",
                "gates `held`: missing [CASH_EXPOSURE_ABOVE_CAP]; unexpected [UNKNOWN_INPUT:economics.revenue.price_per_month]",
                "next_information: expected [], got [economics.revenue.price_per_month]",
            ]
        );
        // A later version is look-ahead: version 2 is the one decided on.
        assert_eq!(r.answer.versions["pass"], 2);
        let mut c2 = case(right());
        c2.candidates[0].as_of = "2026-10-06".parse().unwrap();
        let r = run_case(&c2, &synthetic()).unwrap();
        assert_eq!(
            r.diffs,
            [
                "verdict `pass`: expected PASS, got none (no version on or before as_of)",
                "version `pass`: expected 1, got none",
                "ranked: expected [pass], got []",
            ]
        );

        // Run under another profile: refused.
        let mut other = synthetic();
        other.id = "another-operator".into();
        assert_eq!(
            run_case(&case(right()), &other).unwrap_err().code,
            codes::PROFILE_MISMATCH
        );
    }

    #[test]
    fn look_ahead_and_hold_week_are_checked() {
        // The fact becomes knowable only later the same day: held, and the
        // dropped-record replay agrees.
        let mut c = case(Expected {
            verdicts: BTreeMap::from([
                ("pass".into(), Verdict::Hold),
                ("held".into(), Verdict::Hold),
            ]),
            gates: BTreeMap::from([("pass".into(), vec!["NO_TESTABLE_EVIDENCE".into()])]),
            ranked: vec![],
            next_information: vec!["signals".into(), "economics.revenue.price_per_month".into()],
            versions: BTreeMap::new(),
        });
        c.cited[0].knowable_at = "2026-10-05".parse().unwrap();
        let r = run_case(&c, &synthetic()).unwrap();
        assert!(r.ok, "{:?}", r.diffs);
        assert_eq!(IsoWeek::of(&at()).unwrap().to_string(), "2026-W41");
    }

    #[test]
    fn case_rules() {
        let mut x = right();
        x.verdicts.remove("held");
        x.verdicts.insert("ghost".into(), Verdict::Hold);
        x.ranked = vec!["pass".into(), "pass".into()];
        x.gates.remove("held");
        x.gates
            .insert("pass".into(), vec!["NO_TESTABLE_EVIDENCE".into()]);
        let mut c = case(x);
        c.cited.push(c.cited[0].clone());
        c.candidates[1].id = "pass".into();
        c.candidates[1].version = 1;
        let codes_of: Vec<&str> = validate(&c).unwrap_err().iter().map(|e| e.code).collect();
        assert_eq!(
            codes_of,
            [
                "duplicate",     // a cited record twice
                "duplicate",     // candidate (pass, 1) twice
                "invalid_field", // `ghost` is not a candidate
                "invalid_field", // a PASS candidate with a gate
                "duplicate",     // `pass` ranked twice
            ]
        );
        // A bad candidate is listed under its index.
        let mut c = case(right());
        c.candidates[1].customer = " ".into();
        let e = validate(&c).unwrap_err();
        assert_eq!(
            e[0].to_string(),
            "invalid_field: candidates[1]: customer: must not be empty"
        );
    }
}
