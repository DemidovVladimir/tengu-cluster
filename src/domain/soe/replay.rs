//! O4 historical replay set (roadmap O4 "evaluation set plus at least 8 older
//! dated cases unseen during prompt / policy drafting"): the operator's
//! labelled decision dates and the pure scoring of what a replayed week
//! answered. Pure; the set file is private (`<state>/eval/<set>.toml`, read
//! by the CLI) and the replay runs in `application/soe/replay.rs`.
//!
//! | `soe.replay_set/1` | Value |
//! |---|---|
//! | header | `schema`, `id`, `version` ≥ 1 |
//! | `synthetic` | `true`: an invented fixture; `false`: the operator's labels — private, never in a git work tree |
//! | `profile` | the profile id the labels hold for (`profile_mismatch` under another) |
//! | `note` | what the set is for |
//! | `[[cases]]` | [`ReplayCase`]: ids unique, at least one |
//!
//! | [`ReplayCase`] | Value |
//! |---|---|
//! | `id` | a lineage id |
//! | `decided_at` | the replayed decision: a known instant (RFC 3339 UTC, not a day) |
//! | `week?` | the cycle id; default the ISO week of `decided_at` (UTC); its Monday − 1 day ≤ `decided_at` < its next Monday + 1 day |
//! | `split` | [`Split`] `DEVELOPMENT` · `HOLDOUT` — a holdout label and outcome are shown only after a counted read |
//! | `label` | [`Label`]: `GOOD` (the week held a candidate worth a test) · `BAD` · `STALE` · `CONTRADICTORY` · `HOLD` (acting was wrong) |
//! | `good` · `bad` | opportunity ids the operator judged good / bad (optional, disjoint) |
//! | `outcome_note` | what happened later |
//! | `[[proposals]]` · `[[challenges]]` | recorded stage drafts as a model writes them (`ProposalDraft` / `ChallengeDraft` JSON, record ids in full); they go through the tools' checks |
//!
//! | Score | Rule |
//! |---|---|
//! | Agreement ([`agrees`]) | `GOOD` ⇔ the week ranks a candidate; any other label ⇔ a `HOLD` week (nothing ranked) |
//! | Missed · false positive | a `good` id not ranked · a `bad` id ranked |
//! | `HOLD` precision · recall | held weeks labelled other than `GOOD` / held weeks · held weeks labelled other than `GOOD` / weeks labelled other than `GOOD` |
//! | Rank stability ([`rank_stability`]) | every decided candidate re-assessed under each `rank::tornado(scale)` perturbation (the Critic's holds kept): stable when the ranked ids and their order are unchanged |
//! | Ratios | integer bps, floored; none without a denominator |

// Consumers: `application/soe/replay.rs`, `tengu soe replay`.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::allocate::Week;
use super::portfolio::{IsoWeek, WeeklyPortfolio};
use super::profile::OperatorProfile;
use super::rank::{assess, perturb, rank, tornado, Assessment};
use super::record::{Problems, SoeRecord};
use super::value::{codes, Bps, SchemaTag, ValueError, BPS_FULL};
use crate::domain::lineage::value::Time;

/// Holdout cases the roadmap asks for ("at least 8 older dated cases").
pub const MIN_HOLDOUT_CASES: usize = 8;

const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Split {
    Development,
    Holdout,
}

/// The operator's answer for a week (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Label {
    Good,
    Bad,
    Stale,
    Contradictory,
    Hold,
}

/// One labelled decision date (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayCase {
    pub id: String,
    pub decided_at: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub week: Option<IsoWeek>,
    pub split: Split,
    pub label: Label,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub good: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bad: Vec<String>,
    pub outcome_note: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposals: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub challenges: Vec<Value>,
}

impl ReplayCase {
    /// The cycle id the case replays (module table: `week`).
    pub fn week(&self) -> Option<IsoWeek> {
        self.week.or_else(|| IsoWeek::of(&self.decided_at))
    }

    /// The decision instant, ms.
    pub fn decided_at_ms(&self) -> Option<i64> {
        match self.decided_at {
            Time::At(t) => Some(t),
            _ => None,
        }
    }

    pub fn has_drafts(&self) -> bool {
        !self.proposals.is_empty() || !self.challenges.is_empty()
    }
}

/// `soe.replay_set/1` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaySet {
    pub schema: SchemaTag,
    pub id: String,
    pub version: u32,
    pub synthetic: bool,
    pub profile: String,
    pub note: String,
    pub cases: Vec<ReplayCase>,
}

impl ReplaySet {
    pub fn cases_in(&self, split: Split) -> impl Iterator<Item = &ReplayCase> {
        self.cases.iter().filter(move |c| c.split == split)
    }

    /// `profile_mismatch` when the labels hold for another profile.
    pub fn check_profile(&self, profile: &OperatorProfile) -> Result<(), ValueError> {
        if self.profile == profile.id {
            return Ok(());
        }
        Err(ValueError::new(
            codes::PROFILE_MISMATCH,
            format!(
                "replay set `{}` is labelled for profile `{}`, not `{}`",
                self.id, self.profile, profile.id
            ),
        ))
    }
}

impl SoeRecord for ReplaySet {
    const RECORD: &'static str = "replay_set";

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
        p.id("profile", &self.profile);
        p.text("note", &self.note);
        p.check(
            !self.cases.is_empty(),
            codes::INVALID_FIELD,
            "cases",
            "at least one case",
        );
        p.unique("cases.id", self.cases.iter().map(|c| c.id.as_str()));
        for (i, c) in self.cases.iter().enumerate() {
            let f = |x: &str| format!("cases[{i}].{x}");
            p.id(&f("id"), &c.id);
            p.text(&f("outcome_note"), &c.outcome_note);
            match (c.decided_at_ms(), c.week()) {
                (None, _) => p.push(
                    codes::INVALID_TIME,
                    &f("decided_at"),
                    format_args!(
                        "{}: a known instant (RFC 3339 UTC), not a day",
                        c.decided_at
                    ),
                ),
                (Some(_), None) => p.push(
                    codes::INVALID_TIME,
                    &f("decided_at"),
                    format_args!("{}: no ISO week", c.decided_at),
                ),
                (Some(t), Some(w)) => {
                    let monday = w
                        .monday()
                        .and_hms_opt(0, 0, 0)
                        .map(|d| d.and_utc().timestamp_millis());
                    p.check(
                        monday.is_some_and(|m| m - DAY_MS <= t && t < m + 8 * DAY_MS),
                        codes::INVALID_FIELD,
                        &f("week"),
                        format_args!("{w} does not hold the decision {}", c.decided_at),
                    );
                }
            }
            p.unique_texts(&f("good"), c.good.iter().map(String::as_str));
            p.unique_texts(&f("bad"), c.bad.iter().map(String::as_str));
            for id in c.good.iter().filter(|g| c.bad.contains(g)) {
                p.push(
                    codes::INVALID_FIELD,
                    &f("bad"),
                    format_args!("`{id}` is in good and bad"),
                );
            }
            for (what, drafts) in [("proposals", &c.proposals), ("challenges", &c.challenges)] {
                for (j, d) in drafts.iter().enumerate() {
                    p.check(
                        d.is_object(),
                        codes::INVALID_FIELD,
                        &f(&format!("{what}[{j}]")),
                        "a draft is a table",
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// What a replayed week answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WeekAnswer {
    pub hold: bool,
    /// Ranked ids, in rank order.
    pub ranked: Vec<String>,
}

impl WeekAnswer {
    pub fn of(p: &WeeklyPortfolio) -> WeekAnswer {
        WeekAnswer {
            hold: p.is_hold(),
            ranked: p.ranked.iter().map(|r| r.id.clone()).collect(),
        }
    }
}

/// Module table: does the week agree with the label?
pub fn agrees(label: Label, a: &WeekAnswer) -> bool {
    match label {
        Label::Good => !a.hold,
        _ => a.hold,
    }
}

/// One case scored against its label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaseScore {
    pub label: Label,
    pub agrees: bool,
    /// `good` ids not ranked.
    pub missed: Vec<String>,
    /// `bad` ids ranked.
    pub false_positives: Vec<String>,
}

pub fn score_case(c: &ReplayCase, a: &WeekAnswer) -> CaseScore {
    CaseScore {
        label: c.label,
        agrees: agrees(c.label, a),
        missed: c
            .good
            .iter()
            .filter(|g| !a.ranked.contains(g))
            .cloned()
            .collect(),
        false_positives: c
            .bad
            .iter()
            .filter(|b| a.ranked.contains(b))
            .cloned()
            .collect(),
    }
}

/// `n / d` in bps, floored; none when `d = 0`.
pub fn ratio_bps(n: usize, d: usize) -> Option<Bps> {
    if d == 0 {
        return None;
    }
    let v = (n as u128 * u128::from(BPS_FULL) / d as u128) as i64;
    Bps::new(v.min(i64::from(BPS_FULL))).ok()
}

/// One replayed case as a summary reads it.
#[derive(Debug, Clone, Copy)]
pub struct Scored<'a> {
    pub answer: &'a WeekAnswer,
    pub score: &'a CaseScore,
    pub unsupported_claims: usize,
    pub stability: &'a Stability,
}

/// The cases of one split (module table: score).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub cases: usize,
    pub agree: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agreement_bps: Option<Bps>,
    /// `HOLD` weeks · of them labelled other than `GOOD` · weeks labelled other than `GOOD`.
    pub held: usize,
    pub held_right: usize,
    pub hold_labels: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold_precision_bps: Option<Bps>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold_recall_bps: Option<Bps>,
    pub missed: usize,
    pub false_positives: usize,
    pub unsupported_claims: usize,
    pub perturbations: usize,
    pub stable: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stability_bps: Option<Bps>,
}

pub fn summarize(cases: &[Scored]) -> Summary {
    let n = cases.len();
    let agree = cases.iter().filter(|c| c.score.agrees).count();
    let held = cases.iter().filter(|c| c.answer.hold).count();
    let hold_labels = cases
        .iter()
        .filter(|c| c.score.label != Label::Good)
        .count();
    let held_right = cases
        .iter()
        .filter(|c| c.answer.hold && c.score.label != Label::Good)
        .count();
    let perturbations = cases.iter().map(|c| c.stability.perturbations).sum();
    let stable = cases.iter().map(|c| c.stability.stable).sum();
    Summary {
        cases: n,
        agree,
        agreement_bps: ratio_bps(agree, n),
        held,
        held_right,
        hold_labels,
        hold_precision_bps: ratio_bps(held_right, held),
        hold_recall_bps: ratio_bps(held_right, hold_labels),
        missed: cases.iter().map(|c| c.score.missed.len()).sum(),
        false_positives: cases.iter().map(|c| c.score.false_positives.len()).sum(),
        unsupported_claims: cases.iter().map(|c| c.unsupported_claims).sum(),
        perturbations,
        stable,
        stability_bps: ratio_bps(stable, perturbations),
    }
}

// ---------------------------------------------------------------------------
// Rank stability
// ---------------------------------------------------------------------------

/// One perturbation that moved the ranking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Moved {
    /// `PRICE -2000`.
    pub perturbation: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

/// Module table: how the week's ranking holds under the tornado.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Stability {
    pub scale_bps: i32,
    /// Tornado rows tried (none without a decided candidate).
    pub perturbations: usize,
    pub stable: usize,
    pub moved: Vec<Moved>,
}

fn ranked_ids(all: &[Assessment], profile: &OperatorProfile) -> Vec<String> {
    rank(all, &profile.rank_order)
        .iter()
        .map(|a| a.id.clone())
        .collect()
}

/// Module table: every decided candidate of `week` at each perturbation.
pub fn rank_stability(
    week: &Week,
    profile: &OperatorProfile,
    scale_bps: i32,
) -> Result<Stability, ValueError> {
    let at = week.portfolio.as_of;
    let assessed = |f: &dyn Fn(
        &super::opportunity::Opportunity,
    ) -> Result<super::opportunity::Opportunity, ValueError>|
     -> Result<Vec<Assessment>, ValueError> {
        week.decided
            .iter()
            .map(|d| {
                let o = f(&d.applied.opportunity)?;
                let mut a = assess(&o, &week.cited, profile, at)?;
                a.verdict = d.applied.held(a.verdict);
                Ok(a)
            })
            .collect()
    };
    let mut out = Stability {
        scale_bps,
        ..Stability::default()
    };
    if week.decided.is_empty() {
        return Ok(out);
    }
    let before = ranked_ids(&assessed(&|o| Ok(o.clone()))?, profile);
    for p in tornado(scale_bps) {
        let after = ranked_ids(&assessed(&|o| perturb(o, p).map(|(x, _)| x))?, profile);
        out.perturbations += 1;
        if after == before {
            out.stable += 1;
        } else {
            let name = serde_json::to_value(p.input)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default();
            out.moved.push(Moved {
                perturbation: format!("{name} {:+}", p.scale_bps),
                before: before.clone(),
                after,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::soe::profile::tests::synthetic;
    use crate::domain::soe::record::{from_toml, validate};

    /// The synthetic set (`tests/fixtures/soe/replay.synthetic.toml`).
    pub(crate) const SET: &str = include_str!("../../../tests/fixtures/soe/replay.synthetic.toml");

    fn answer(hold: bool, ranked: &[&str]) -> WeekAnswer {
        WeekAnswer {
            hold,
            ranked: ranked.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn set_rules() {
        let s: ReplaySet = from_toml(SET).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(s.cases_in(Split::Holdout).count(), 1);
        assert_eq!(s.cases[0].week().unwrap().to_string(), "2026-W41");
        assert!(s.check_profile(&synthetic()).is_ok());
        let mut other = synthetic();
        other.id = "someone-else".into();
        assert_eq!(
            s.check_profile(&other).unwrap_err().code,
            codes::PROFILE_MISMATCH
        );

        let mut bad = s.clone();
        bad.cases[1].id = "dev-hold".into();
        bad.cases[0].decided_at = "2026-10-05".parse().unwrap();
        bad.cases[1].bad.push("a".into());
        bad.cases[2].week = Some("2026-W30".parse().unwrap());
        bad.cases[2]
            .proposals
            .push(Value::String("not a table".into()));
        let got: Vec<String> = validate(&bad)
            .unwrap_err()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            got,
            [
                "duplicate: cases.id: \"dev-hold\" twice",
                "invalid_time: cases[0].decided_at: 2026-10-05: a known instant (RFC 3339 UTC), not a day",
                "invalid_field: cases[1].bad: `a` is in good and bad",
                "invalid_field: cases[2].week: 2026-W30 does not hold the decision 2026-09-07T12:00:00Z",
                "invalid_field: cases[2].proposals[0]: a draft is a table",
            ]
        );
    }

    #[test]
    fn scores_and_summary() {
        let s: ReplaySet = from_toml(SET).unwrap();
        let (hold, good) = (&s.cases[0], &s.cases[1]);
        // A HOLD week agrees with HOLD, not with GOOD.
        let held = answer(true, &[]);
        assert!(score_case(hold, &held).agrees);
        let sc = score_case(good, &held);
        assert!(!sc.agrees);
        assert_eq!(sc.missed, ["a"]);
        // Ranking the bad one is a false positive.
        let ranked = answer(false, &["b", "a"]);
        let sc2 = score_case(good, &ranked);
        assert!(sc2.agrees && sc2.missed.is_empty());
        assert_eq!(sc2.false_positives, ["b"]);

        let stable = Stability {
            scale_bps: 2000,
            perturbations: 8,
            stable: 6,
            moved: vec![],
        };
        let none = Stability::default();
        let s1 = score_case(hold, &held);
        let rows = [
            Scored {
                answer: &held,
                score: &s1,
                unsupported_claims: 1,
                stability: &none,
            },
            Scored {
                answer: &held,
                score: &sc,
                unsupported_claims: 0,
                stability: &stable,
            },
        ];
        let m = summarize(&rows);
        assert_eq!((m.cases, m.agree, m.held, m.held_right), (2, 1, 2, 1));
        assert_eq!(m.agreement_bps, Bps::new(5000).ok());
        assert_eq!(m.hold_precision_bps, Bps::new(5000).ok());
        assert_eq!(m.hold_recall_bps, Bps::new(10_000).ok());
        assert_eq!(
            (m.missed, m.false_positives, m.unsupported_claims),
            (1, 0, 1)
        );
        assert_eq!(m.stability_bps, Bps::new(7500).ok());
        // No denominator, no ratio.
        let empty = summarize(&[]);
        assert_eq!((empty.agreement_bps, empty.stability_bps), (None, None));
        assert_eq!(ratio_bps(1, 3), Bps::new(3333).ok());
    }

    #[test]
    fn rank_stability_tries_every_tornado_row() {
        use crate::domain::soe::allocate::tests::active_vs_new;
        use crate::domain::soe::record::Tier;
        let w = active_vs_new(Tier::Medium, Tier::Medium);
        let ranked: Vec<String> = w.portfolio.ranked.iter().map(|r| r.id.clone()).collect();
        assert_eq!(ranked, ["act", "new"]);
        let s = rank_stability(&w, &synthetic(), 2000).unwrap();
        assert_eq!((s.scale_bps, s.perturbations), (2000, 8));
        assert_eq!(s.stable + s.moved.len(), 8);
        for m in &s.moved {
            assert_eq!(m.before, ranked, "{m:?}");
            assert_ne!(m.after, m.before, "{m:?}");
        }
        // Evidence ranks first in the synthetic order and no tornado row
        // moves it: a small scale keeps the order.
        let small = rank_stability(&w, &synthetic(), 1).unwrap();
        assert_eq!(small.stable, 8, "{small:?}");
        // A week without a candidate tries nothing.
        let mut empty = w.clone();
        empty.decided.clear();
        assert_eq!(
            rank_stability(&empty, &synthetic(), 2000).unwrap(),
            Stability {
                scale_bps: 2000,
                ..Stability::default()
            }
        );
    }
}
