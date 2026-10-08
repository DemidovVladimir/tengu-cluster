//! Capability fit (PRD § 8 step 4 "Match"; `docs/soe-2026-10-08.md` § 5):
//! how the operator's capabilities in the signed profile cover the skills an
//! opportunity requires, at the decision time. Pure; the one fit rule —
//! ranking (`rank.rs`, `CAPABILITY_FIT`) and the O3 allocation reuse it.
//!
//! | Rule | Value |
//! |---|---|
//! | [`active_at`] | the capability's `as_of` surely not after the decision (a day against an instant inside it is not) and the decision before the end of `valid_until` (a day covers its whole day) |
//! | match | `OperatorCapability.skill` equals the required skill (exact) |
//!
//! | [`FitLevel`] (one skill) | When |
//! |---|---|
//! | `PROVEN` | an active `PROVEN` capability |
//! | `CLAIMED` | an active capability, `CLAIMED` or `UNKNOWN` level |
//! | `STALE` | capabilities name the skill, none active (expired, or not yet valid) |
//! | `MISSING` | no capability names the skill |
//!
//! The opportunity's fit is its weakest skill's level; `UNSTATED` when it
//! lists no `requires_skills` (ranks as unknown — last — never as a fit).

use serde::Serialize;

use super::opportunity::Opportunity;
use super::profile::{CapabilityLevel, OperatorCapability, OperatorProfile};
use crate::domain::lineage::value::{Time, TimeOrder};

/// How well one skill — or the whole opportunity — is covered (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FitLevel {
    Proven,
    Claimed,
    Stale,
    Missing,
    /// The opportunity lists no required skill.
    Unstated,
}

impl FitLevel {
    /// `PROVEN` 4 … `MISSING` 1; `UNSTATED` none (never ranks as a level).
    pub fn level(self) -> Option<u8> {
        match self {
            FitLevel::Proven => Some(4),
            FitLevel::Claimed => Some(3),
            FitLevel::Stale => Some(2),
            FitLevel::Missing => Some(1),
            FitLevel::Unstated => None,
        }
    }
}

/// One required skill and what covers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillFit {
    pub skill: String,
    pub level: FitLevel,
    /// The capability behind `level` (the lowest id at it); none when missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
}

/// The opportunity's fit: its weakest skill, and every skill in
/// `requires_skills` order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityFit {
    pub level: FitLevel,
    pub skills: Vec<SkillFit>,
}

/// Module table: whether `c` holds at `at`.
pub fn active_at(c: &OperatorCapability, at: &Time) -> bool {
    let started = c.as_of.order(at) == TimeOrder::NotAfter;
    let unexpired = matches!(
        (at.latest(), c.valid_until.window_end()),
        (Some(t), Some(end)) if t < end
    );
    started && unexpired
}

fn skill_fit(skill: &str, profile: &OperatorProfile, at: &Time) -> SkillFit {
    let mut best: Option<(FitLevel, &str)> = None;
    for c in profile.capabilities.iter().filter(|c| c.skill == skill) {
        let level = match (active_at(c, at), c.level) {
            (true, CapabilityLevel::Proven) => FitLevel::Proven,
            (true, _) => FitLevel::Claimed,
            (false, _) => FitLevel::Stale,
        };
        // `FitLevel` orders best first; ties keep the lowest id.
        let candidate = (level, c.id.as_str());
        if best.map_or(true, |b| candidate < b) {
            best = Some(candidate);
        }
    }
    match best {
        Some((level, id)) => SkillFit {
            skill: skill.to_string(),
            level,
            capability: Some(id.to_string()),
        },
        None => SkillFit {
            skill: skill.to_string(),
            level: FitLevel::Missing,
            capability: None,
        },
    }
}

/// Module table: how `profile`'s capabilities cover `opp`'s skills at `at`.
pub fn fit(opp: &Opportunity, profile: &OperatorProfile, at: &Time) -> CapabilityFit {
    let skills: Vec<SkillFit> = opp
        .requires_skills
        .iter()
        .map(|s| skill_fit(s, profile, at))
        .collect();
    let level = skills
        .iter()
        .map(|s| s.level)
        .max()
        .unwrap_or(FitLevel::Unstated);
    CapabilityFit { level, skills }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::soe::opportunity::tests::recurring;
    use crate::domain::soe::profile::tests::synthetic;

    fn t(s: &str) -> Time {
        s.parse().unwrap()
    }

    #[test]
    fn active_at_bounds() {
        let p = synthetic();
        let c = &p.capabilities[0]; // 2026-09-01 .. 2027-09-01
        assert!(active_at(c, &t("2026-10-05T12:00:00Z")));
        // Its first day is not surely after `as_of`: active from the next.
        assert!(!active_at(c, &t("2026-09-01T12:00:00Z")));
        assert!(active_at(c, &t("2026-09-02")));
        // `valid_until` covers its whole day.
        assert!(active_at(c, &t("2027-09-01T23:59:59Z")));
        assert!(active_at(c, &t("2027-09-01")));
        assert!(!active_at(c, &t("2027-09-02T00:00:00Z")));
        assert!(!active_at(c, &Time::Unknown));
        let mut open = c.clone();
        open.valid_until = Time::Unknown;
        assert!(!active_at(&open, &t("2026-10-05T12:00:00Z")));
    }

    #[test]
    fn fit_is_the_weakest_skill() {
        let p = synthetic();
        let at = t("2026-10-05T12:00:00Z");
        let mut o = recurring(); // requires ["integration"]
        let f = fit(&o, &p, &at);
        assert_eq!(f.level, FitLevel::Proven);
        assert_eq!(f.skills[0].capability.as_deref(), Some("rust-integration"));
        o.requires_skills = vec!["integration".into(), "automation".into()];
        assert_eq!(fit(&o, &p, &at).level, FitLevel::Claimed);
        // `migration` is PROVEN but expired 2026-06-30.
        o.requires_skills.push("migration".into());
        let f = fit(&o, &p, &at);
        assert_eq!(f.level, FitLevel::Stale);
        assert_eq!(
            f.skills
                .iter()
                .map(|s| (s.skill.as_str(), s.level))
                .collect::<Vec<_>>(),
            [
                ("integration", FitLevel::Proven),
                ("automation", FitLevel::Claimed),
                ("migration", FitLevel::Stale)
            ]
        );
        // Valid again before it expired.
        assert_eq!(
            fit(&o, &p, &t("2026-05-04T12:00:00Z")).skills[2].level,
            FitLevel::Proven
        );
        o.requires_skills.push("design".into());
        let f = fit(&o, &p, &at);
        assert_eq!(
            (f.level, f.skills[3].capability.clone()),
            (FitLevel::Missing, None)
        );
        o.requires_skills.clear();
        assert_eq!(fit(&o, &p, &at).level, FitLevel::Unstated);
        assert_eq!(FitLevel::Unstated.level(), None);
        assert!(FitLevel::Proven.level() > FitLevel::Missing.level());
        // An active PROVEN capability beats a stale one for the same skill.
        let mut p2 = p.clone();
        let mut fresh = p2.capabilities[2].clone();
        fresh.id = "migration-2026".into();
        fresh.as_of = t("2026-07-01");
        fresh.valid_until = t("2027-07-01");
        p2.capabilities.push(fresh);
        o.requires_skills = vec!["migration".into()];
        let f = fit(&o, &p2, &at);
        assert_eq!(
            (f.level, f.skills[0].capability.as_deref()),
            (FitLevel::Proven, Some("migration-2026"))
        );
    }
}
