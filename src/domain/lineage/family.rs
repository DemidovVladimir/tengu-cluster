//! Hypothesis family — `lineage/families/<id>.toml` (`docs/lineage-2026-10-06.md`
//! § 2, roadmap P3): one idea (Rule W, its placebo, each rejected idea) and
//! how much searching happened outside the registry.
//!
//! | Field | Value |
//! |---|---|
//! | `hypothesis`, `mechanism?` | what is believed and why it should pay |
//! | `role` | `PRIMARY` · `NEGATIVE_CONTROL` · `PLACEBO` |
//! | `status`, `status_reason` | `OPEN` · `SURVIVING` · `REJECTED` · `INCONCLUSIVE` · `RETIRED` |
//! | `origin_at`, `origin`, `origin_by` | when, where (a locator) and by whom it arose |
//! | `parent` | a family id · `NONE` · `UNKNOWN` |
//! | `preceded_by[]`, `controls[]` | family ids: the ideas before it, its controls |
//! | `[prior_search] count, precision?, source, note?` | attempts made outside the registry (multiple testing) |

use serde::{Deserialize, Serialize};

use super::value::{Count, EvidenceRef, Locator, Precision, Time};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FamilyRole {
    Primary,
    NegativeControl,
    Placebo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FamilyStatus {
    Open,
    Surviving,
    Rejected,
    Inconclusive,
    Retired,
}

/// Attempts outside the registry (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriorSearch {
    pub count: Count,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision: Option<Precision>,
    pub source: Locator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `lineage/families/<id>.toml` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Family {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub hypothesis: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mechanism: Option<String>,
    pub role: FamilyRole,
    pub status: FamilyStatus,
    pub status_reason: String,
    pub origin_at: Time,
    pub origin: Locator,
    pub origin_by: String,
    pub parent: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preceded_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_search: Option<PriorSearch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRef>,
}
