//! Lineage use cases (`docs/lineage-2026-10-06.md` § 5): `tengu lineage
//! verify` and the views that need more than the registry — pins, evidence
//! files, run dirs — through `ports/lineage.rs`. The registry itself, its
//! checks and its queries are pure (`domain/lineage/`).
//!
//! | Module | Does |
//! |---|---|
//! | `verify` | [`verify::verify`]: `Registry::validate` + `--pins` (`pin_drift`, `pin_unresolved`, `unknown_binding`) + `--evidence` (`evidence_missing`, `evidence_mismatch`, `mutable_evidence`, `result_mismatch`, `extract_unsupported`); [`verify::pin_status`]: a generation's pins OK / DRIFT / UNRESOLVED |
//! | `attempts` | [`attempts::scan`]: every run dir + holdout read of the given state dirs |

pub(crate) mod attempts;
pub(crate) mod verify;

#[cfg(test)]
mod tests;
