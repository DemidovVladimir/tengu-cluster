//! Lineage registry — experiments, variants, Experience, capabilities,
//! generations (`docs/lineage-2026-10-06.md`, `TENGU_ROADMAP.md` P2–P5). Pure:
//! the record types, the [`Registry`] of every record with its checks
//! ([`Registry::validate`]), the queries (`trace`, `family_report`,
//! `acceptance`) and the pin hashes. The files are read by `config/lineage.rs`;
//! pins, evidence and run dirs are resolved through `ports/lineage.rs`.
//!
//! | Module | Holds |
//! |---|---|
//! | `value` | ids, `Time`, `Count`, `Locator`, `EvidenceRef`, `PinTarget`, `Binding` |
//! | `family` · `variant` · `experiment` · `episode` · `incident` · `capability` · `generation` · `locks` | one record type each (`lineage/<dir>/<id>.toml`, `locks.toml`); evidence records are `domain/evidence.rs`'s `EvidenceRecord` |
//! | `registry` | [`Registry`], [`Registry::validate`] (the codes below), the locator walk |
//! | `query` · `acceptance` | `trace`, `family_report` (search accounting), `attempt_rows` · the 21 Rule-W answers (handoff § 57) |
//! | `pins` | record digests and pin hashes |
//!
//! | Code (`validate`) | Severity | Fires on |
//! |---|---|---|
//! | `invalid_id` | Error | an id outside `^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$` |
//! | `duplicate_id` | Error | one id used by two records (any kinds: `show` / `trace` take a bare id) |
//! | `invalid_field` | Error | a shape rule: title empty or > 160 chars, a hash not 64 hex, `[spec]` not exactly one shape, `version` 0, a window ending before it starts, `ci95` low > high, a FROZEN generation without `frozen_at`, alternatives without exactly one chosen, an OPERATIONAL_INCIDENT episode without incidents, a loop in variant parents, an evidence record's own rules |
//! | `dangling_ref` | Error | a reference (`family`, `variant`, `generation`, `parent`, `preceded_by`, `controls`, `capabilities`, `incidents`, `record:` locators, lock rows) to no record; `UNKNOWN` / `NONE` (`ROOT` for a variant parent) exempt |
//! | `inconsistent_ref` | Error | a variant of another family than the record naming both |
//! | `capability_version_missing` | Error | a generation names a capability version the registry does not hold |
//! | `binding_conflict` | Error | one binding owned by two capabilities |
//! | `future_leakage` | Error · Warn | an episode's information unknown-timed or after its decision (Warn: the same day, undecidable); a decision with information but an UNKNOWN time |
//! | `holdout_missing` | Error | a HOLDOUT-class result, or a HOLDOUT_TEST experiment, without a HOLDOUT window |
//! | `holdout_overlaps_development` | Error | DEVELOPMENT and HOLDOUT windows overlapping in time without `split_by = "INSTRUMENTS"` |
//! | `forward_incomplete` | Error | a FORWARD_PAPER experiment without a FORWARD window, a `validity` or FORWARD_PAPER evidence |
//! | `frozen_manifest_changed` | Error | a generation whose file digest differs from its last `[[frozen]]` row; a FROZEN one without a row |
//! | `seal_mismatch` | Error · Warn | a sealed record changed since, or sealed after its first outcome (Warn: same day); a preregistered record neither sealed nor carrying a `prereg` evidence ref |
//!
//! Codes of `tengu lineage verify --pins / --evidence` (`application/lineage/verify.rs`):
//! `pin_drift`, `pin_unresolved`, `unknown_binding`, `evidence_missing`,
//! `evidence_mismatch`, `mutable_evidence`, `evidence_planned`, `result_mismatch`,
//! `extract_unsupported`; of the loader (`config/lineage.rs`): `load_error`.

pub(crate) mod acceptance;
pub(crate) mod capability;
pub(crate) mod episode;
pub(crate) mod experiment;
pub(crate) mod family;
pub(crate) mod generation;
pub(crate) mod incident;
pub(crate) mod locks;
pub(crate) mod pins;
pub(crate) mod query;
pub(crate) mod registry;
mod validate;
pub(crate) mod value;
pub(crate) mod variant;

use std::fmt;

use serde::Serialize;

pub(crate) use registry::Registry;

/// How bad a [`Finding`] is: any `Error` fails `tengu lineage verify`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Severity {
    Error,
    Warn,
    Info,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Severity::Error => "ERROR",
            Severity::Warn => "WARN",
            Severity::Info => "INFO",
        })
    }
}

/// One verification finding: `record` = `<kind>/<id>` (or a file).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub code: String,
    pub record: String,
    pub message: String,
}

impl Finding {
    pub fn new(
        severity: Severity,
        code: &str,
        record: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Finding {
            severity,
            code: code.to_string(),
            record: record.into(),
            message: message.into(),
        }
    }

    pub fn error(code: &str, record: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, record, message)
    }

    pub fn warn(code: &str, record: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(Severity::Warn, code, record, message)
    }
}

/// Any `Error` among `findings`.
pub fn has_errors(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.severity == Severity::Error)
}
