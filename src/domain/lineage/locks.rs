//! `lineage/locks.toml` (`docs/lineage-2026-10-06.md` § 2): frozen
//! generations and sealed preregistrations — append-only rows.
//!
//! | Row | Fields | Rule (`Registry::validate`) |
//! |---|---|---|
//! | `[[frozen]]` | `generation`, `manifest_sha256`, `frozen_at`, `commit?` | the generation's last row equals `Registry::frozen_digest` — the canonical sha256 of its file's digest (TOML → JSON, `pins::toml_digest`) with the digest of every capability record it lists — else `frozen_manifest_changed`; a `FROZEN` generation without a row too |
//! | `[[sealed]]` | `record` (`variant:<id>` · `experiment:<id>`), `sha256`, `sealed_at` | the record's file digest now = `sha256`, and `sealed_at` ≤ its first outcome (experiment `ran_at`, a forward experiment's FORWARD window **start** — outcomes accrue from the first entry; a variant: the earliest of its experiments'), else `seal_mismatch`; an outcome time UNKNOWN: `tengu lineage seal` refuses, a seal row warns |

use serde::{Deserialize, Serialize};

use super::value::Time;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frozen {
    pub generation: String,
    pub manifest_sha256: String,
    pub frozen_at: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sealed {
    pub record: String,
    pub sha256: String,
    pub sealed_at: Time,
}

/// `lineage/locks.toml` (module table).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Locks {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frozen: Vec<Frozen>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sealed: Vec<Sealed>,
}

/// The `[[sealed]]` row `tengu lineage seal` appends (module table).
pub fn sealed_row(record: &str, sha256: &str, sealed_at: &Time) -> String {
    format!(
        "\n[[sealed]]\nrecord = \"{record}\"\nsha256 = \"{sha256}\"\nsealed_at = \"{sealed_at}\"\n"
    )
}
