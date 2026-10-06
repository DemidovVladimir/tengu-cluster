//! Incident — `lineage/incidents/<id>.toml` (`docs/lineage-2026-10-06.md`
//! § 2, roadmap P0 / P4): an operational or data incident, kept apart from
//! strategy failure.
//!
//! | Field | Value |
//! |---|---|
//! | `class` | `INFRASTRUCTURE` `NETWORK` `VENUE` `DATA` `EXECUTION` `HOST` |
//! | `started_at`, `ended_at`, `detected_by` | times (`ended_at` not before `started_at`); who / what saw it |
//! | `generation`, `experiment?` | ids |
//! | `strategy_impact`, `strategy_impact_note` | `NONE` `DEGRADED` `MATERIAL` `UNKNOWN` |
//! | `[[data_impact]] stream, from, to, after, backfill?, note?` | a stream's gap and what it became: `MISSING` `BACKFILLED` `LIVE_RECORDED` |

use serde::{Deserialize, Serialize};

use super::value::{EvidenceRef, Locator, Time};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IncidentClass {
    Infrastructure,
    Network,
    Venue,
    Data,
    Execution,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StrategyImpact {
    None,
    Degraded,
    Material,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DataAfter {
    Missing,
    Backfilled,
    LiveRecorded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataImpact {
    pub stream: String,
    pub from: Time,
    pub to: Time,
    pub after: DataAfter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backfill: Option<Locator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `lineage/incidents/<id>.toml` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Incident {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub class: IncidentClass,
    pub started_at: Time,
    pub ended_at: Time,
    pub detected_by: String,
    pub generation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment: Option<String>,
    pub strategy_impact: StrategyImpact,
    pub strategy_impact_note: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_impact: Vec<DataImpact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRef>,
}
