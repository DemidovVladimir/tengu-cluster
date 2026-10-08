//! Opportunity episode — the outcome and lesson of one decided candidate
//! (PRD § 6 `OpportunityEpisode`, § 8 step 10: append, never erase a
//! failure). Private: stored under `<TENGU_HOME>/state/soe/`, never in the
//! public `lineage/` registry. Reuses the lineage Experience semantics:
//! `[quality]` and `[lesson]` are `lineage::episode::{Quality, Lesson}`, and
//! the quadrant is `lineage::episode::quadrant_of` — one rule for both.
//!
//! | Field | Value |
//! |---|---|
//! | header | `schema = "soe.opportunity_episode/1"`, `id`, `version` ≥ 1 |
//! | `opportunity`, `opportunity_version` | the candidate id and the version decided on |
//! | `profile_sha256` | digest of the profile in force (64 hex, in full) |
//! | `decided_at`, `verdict` | known time · `PASS` `HOLD` `REJECT` |
//! | `currency` | of `spend` and every amount below |
//! | `[[actions]]` | `kind` (`ApprovalKind`), `at` (not before `decided_at`), `approved_by` |
//! | `spend`, `owner_hours` | ≥ 0 · hours |
//! | `[forecast_base]` · `[actual]` | `monthly_cash`, `owner_hours` (+ `time_adjusted` in the forecast): `Est` — a known figure is a point, a missing one `"UNKNOWN"` |
//! | `surprise?`, `evidence_strength` | text · `LOW` `MEDIUM` `HIGH` `UNKNOWN` |
//! | `[quality]` · `[lesson]?` | lineage episode `Quality` (decision × outcome stored apart) · `Lesson` |

// Consumers land with the cycle (O3 `run_cycle`, private `episodes.jsonl`).
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use super::experiment::ApprovalKind;
use super::record::{Problems, SoeRecord, Tier, Verdict};
use super::value::{Currency, Est, Minor, SchemaTag};
use crate::domain::lineage::episode::{quadrant_of, Lesson, Quadrant, Quality};
use crate::domain::lineage::value::Time;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpisodeAction {
    pub kind: ApprovalKind,
    pub at: Time,
    pub approved_by: String,
}

/// The base scenario as forecast when decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forecast {
    pub monthly_cash: Est<Minor>,
    pub time_adjusted: Est<Minor>,
    pub owner_hours: Est<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Actual {
    pub monthly_cash: Est<Minor>,
    pub owner_hours: Est<u32>,
}

/// `soe.opportunity_episode/1` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpportunityEpisode {
    pub schema: SchemaTag,
    pub id: String,
    pub version: u32,
    pub opportunity: String,
    pub opportunity_version: u32,
    pub profile_sha256: String,
    pub decided_at: Time,
    pub verdict: Verdict,
    pub currency: Currency,
    pub actions: Vec<EpisodeAction>,
    pub spend: Minor,
    pub owner_hours: u32,
    pub forecast_base: Forecast,
    pub actual: Actual,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surprise: Option<String>,
    pub evidence_strength: Tier,
    pub quality: Quality,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lesson: Option<Lesson>,
}

impl OpportunityEpisode {
    /// Decision quality × outcome (`lineage::episode::quadrant_of`).
    pub fn quadrant(&self) -> Quadrant {
        quadrant_of(self.quality.decision, self.quality.outcome)
    }
}

impl SoeRecord for OpportunityEpisode {
    const RECORD: &'static str = "opportunity_episode";

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
        p.id("opportunity", &self.opportunity);
        p.check(
            self.opportunity_version >= 1,
            super::value::codes::INVALID_VERSION,
            "opportunity_version",
            "must be ≥ 1",
        );
        p.sha256("profile_sha256", &self.profile_sha256);
        p.known("decided_at", &self.decided_at);
        for (i, a) in self.actions.iter().enumerate() {
            p.text(&format!("actions[{i}].approved_by"), &a.approved_by);
            p.known(&format!("actions[{i}].at"), &a.at);
            p.not_after(
                "decided_at",
                &self.decided_at,
                &format!("actions[{i}].at"),
                &a.at,
            );
        }
        p.non_negative("spend", self.spend);
        if let Some(s) = &self.surprise {
            p.text("surprise", s);
        }
        p.text("quality.decision_note", &self.quality.decision_note);
        if let Some(l) = &self.lesson {
            p.text("lesson.text", &l.text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::lineage::episode::{DecisionQuality, OutcomeQuality};
    use crate::domain::soe::record::{from_toml, validate};

    const EPISODE: &str = r#"
schema = "soe.opportunity_episode/1"
id = "example-automation.2026-W41"
version = 1
opportunity = "example-automation"
opportunity_version = 1
profile_sha256 = "0000000000000000000000000000000000000000000000000000000000000002"
decided_at = "2026-10-05T10:00:00Z"
verdict = "PASS"
currency = "EUR"
spend = "250.00"
owner_hours = 9
surprise = "two firms asked for a fixed-price setup instead"
evidence_strength = "MEDIUM"

[[actions]]
kind = "CONTACT"
at = "2026-10-06T09:00:00Z"
approved_by = "operator"

[forecast_base]
monthly_cash = { low = "410.00", base = "410.00", high = "410.00" }
time_adjusted = "UNKNOWN: price unknown"
owner_hours = { low = 10, base = 10, high = 10 }

[actual]
monthly_cash = "UNKNOWN: no pilot billed yet"
owner_hours = { low = 9, base = 9, high = 9 }

[quality]
decision = "SUPPORTED"
decision_note = "two primary records, bounded first stage"
execution = "CLEAN"
outcome = "UNKNOWN"
"#;

    #[test]
    fn episode_parses_and_actions_follow_the_decision() {
        let e: OpportunityEpisode = from_toml(EPISODE).unwrap();
        assert_eq!(e.quadrant(), Quadrant::Unknown);
        assert_eq!(e.actual.monthly_cash.base(), None);
        let back: OpportunityEpisode = from_toml(&toml::to_string(&e).unwrap()).unwrap();
        assert_eq!(back, e);
        let mut early = e.clone();
        early.actions[0].at = "2026-10-04T09:00:00Z".parse().unwrap();
        early.profile_sha256 = "abc".into();
        let codes: Vec<_> = validate(&early)
            .unwrap_err()
            .iter()
            .map(|x| x.code)
            .collect();
        assert_eq!(codes, vec!["invalid_field", "future_leakage"]);
        assert!(
            from_toml::<OpportunityEpisode>(&format!("deal_terms = \"x\"\n{EPISODE}")).is_err()
        );
    }

    #[test]
    fn quadrant_matches_lineage_rule() {
        let mut e: OpportunityEpisode = from_toml(EPISODE).unwrap();
        let decisions = [
            DecisionQuality::Supported,
            DecisionQuality::Unsupported,
            DecisionQuality::Unknown,
        ];
        let outcomes = [
            OutcomeQuality::Favorable,
            OutcomeQuality::Unfavorable,
            OutcomeQuality::Neutral,
            OutcomeQuality::Unknown,
        ];
        for d in decisions {
            for o in outcomes {
                e.quality.decision = d;
                e.quality.outcome = o;
                assert_eq!(e.quadrant(), quadrant_of(d, o), "{d:?} × {o:?}");
            }
        }
        // The rule itself (lineage module table): luck stays visible.
        let q = |d, o| quadrant_of(d, o).name();
        assert_eq!(
            q(DecisionQuality::Supported, OutcomeQuality::Favorable),
            "GOOD_DECISION_GOOD_OUTCOME"
        );
        assert_eq!(
            q(DecisionQuality::Supported, OutcomeQuality::Unfavorable),
            "GOOD_DECISION_BAD_OUTCOME"
        );
        assert_eq!(
            q(DecisionQuality::Unsupported, OutcomeQuality::Favorable),
            "BAD_DECISION_GOOD_OUTCOME"
        );
        assert_eq!(
            q(DecisionQuality::Unsupported, OutcomeQuality::Unfavorable),
            "BAD_DECISION_BAD_OUTCOME"
        );
        assert_eq!(
            q(DecisionQuality::Supported, OutcomeQuality::Neutral),
            "UNKNOWN"
        );
        assert_eq!(
            q(DecisionQuality::Unknown, OutcomeQuality::Favorable),
            "UNKNOWN"
        );
    }
}
