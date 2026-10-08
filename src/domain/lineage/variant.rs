//! Variant — `lineage/variants/<id>.toml` (`docs/lineage-2026-10-06.md` § 2,
//! roadmap P3): one materially different version of a family's strategy
//! (Sunday vs Saturday entry, top-4, a liquidity cut …) — what was searched.
//!
//! | Field | Value |
//! |---|---|
//! | `family`, `parent` | its family; a variant id of it or `ROOT` |
//! | `[[changed]] dim, from, to` | the dimensions changed from the parent (any scalar) |
//! | `reason`, `registered_at`, `order?` | why (`UNKNOWN` allowed), when registered, the search order when known |
//! | `preregistered` | registered before its outcome: a `[[sealed]]` lock row, or (before the registry) an `[[evidence]]` with `role = "prereg"` |
//! | `status` | `REGISTERED` · `ACTIVE` · `SURVIVING` · `REJECTED` · `INCONCLUSIVE` · `SUPERSEDED` |
//! | `[spec]` | exactly one shape ([`VariantSpec::shape`]): `{sandbox, strategy, spec_sha256}` (a `[backtest.strategies.<strategy>]`) · `{spec_sha256, source}` (seen only in a run) · `{described}` (not a spec) |

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::value::{EvidenceRef, Locator, Time};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VariantStatus {
    Registered,
    Active,
    Surviving,
    Rejected,
    Inconclusive,
    Superseded,
}

/// One changed dimension.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub dim: String,
    pub from: Value,
    pub to: Value,
}

/// The variant's spec (module table); [`VariantSpec::shape`] checks it is
/// exactly one shape.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Locator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub described: Option<String>,
}

/// The shape a [`VariantSpec`] takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecShape {
    Library,
    Run,
    Described,
}

impl VariantSpec {
    /// The one shape it takes, else why not.
    pub fn shape(&self) -> Result<SpecShape, String> {
        let s = self;
        match (
            s.sandbox.is_some(),
            s.strategy.is_some(),
            s.spec_sha256.is_some(),
            s.source.is_some(),
            s.described.is_some(),
        ) {
            (true, true, true, false, false) => Ok(SpecShape::Library),
            (false, false, true, true, false) => Ok(SpecShape::Run),
            (false, false, false, false, true) => Ok(SpecShape::Described),
            _ => Err(
                "[spec] takes exactly one of {sandbox, strategy, spec_sha256} · \
                 {spec_sha256, source} · {described}"
                    .into(),
            ),
        }
    }
}

/// `lineage/variants/<id>.toml` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub family: String,
    pub parent: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<Change>,
    pub reason: String,
    pub registered_at: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<u32>,
    pub preregistered: bool,
    pub status: VariantStatus,
    pub spec: VariantSpec,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRef>,
}
