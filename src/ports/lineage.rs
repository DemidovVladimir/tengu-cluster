//! Lineage ports (`docs/lineage-2026-10-06.md` § 2, § 5): what `tengu lineage
//! verify --pins / --evidence`, `family` and `attempts` need from outside
//! the registry. Impls: `adapters/outbound/lineage/`.
//!
//! | Port | Answers |
//! |---|---|
//! | [`ContractProbe`] | a pin target's sha256 now; whether a catalog tool / a strategy kind exists; a repo sandbox's `[generation]` ([`SandboxBinding`]) |
//! | [`EvidenceResolver`] | a locator → present (path, sha256 now, the sha256 the vault records) · missing · nothing to open |
//! | [`ResultSource`] | an `extract` on a resolved evidence file → the recomputed figures; `None` = not this source's kind (plug and play: one source per kind — `arm:` / `gate` from `report.json`, `ledger:` later) |
//! | [`AttemptSource`] | the run dirs and holdout reads of a state dir |

use std::path::{Path, PathBuf};

use crate::domain::lineage::query::{HoldoutRead, RunAttempt};
use crate::domain::lineage::value::{Locator, PinTarget};

/// A sandbox's `[generation]` as its repo config file declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxBinding {
    /// `[generation] id`.
    pub generation: String,
    /// `[generation] registry` as written.
    pub registry: String,
    /// It resolves (relative to the config file) to the registry verified.
    pub same_registry: bool,
}

/// Pin targets and bindings (module table).
pub trait ContractProbe {
    /// The target's sha256 now (`domain/lineage/pins.rs`); `Err` = why it
    /// does not resolve.
    fn pin_sha256(&self, target: &PinTarget) -> Result<String, String>;
    fn tool_exists(&self, name: &str) -> bool;
    fn strategy_kind_exists(&self, kind: &str) -> bool;
    /// `[generation]` of `<repo>/sandboxes/<sandbox>/config.toml`; `Ok(None)`
    /// = the file binds none; `Err` = it does not read or parse.
    fn sandbox_binding(&self, sandbox: &str) -> Result<Option<SandboxBinding>, String>;
}

/// What a locator resolved to.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Present {
        path: PathBuf,
        /// The file's sha256 now (a dir: its tree hash); `None` = not hashed.
        sha256: Option<String>,
        /// What the vault records for it (the evidence record's item, else
        /// the vault's `MANIFEST.json`), when it is a vault copy.
        recorded: Option<String>,
        /// Live state: may change under the record (`mutable_evidence`).
        mutable: bool,
    },
    Missing(String),
    /// `url:`, `record:`, `UNKNOWN`, an unchecked `git:` — nothing to open.
    NotAFile,
}

/// Locator → file (module table).
pub trait EvidenceResolver {
    fn resolve(&self, locator: &Locator) -> Resolution;
}

/// Figures recomputed from a source.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extracted {
    pub n: Option<u64>,
    pub mean_net_bps: Option<f64>,
    pub ci95_bps: Option<[f64; 2]>,
    pub net_usd: Option<f64>,
    pub t_stat: Option<f64>,
}

/// One kind of `extract` (module table).
pub trait ResultSource {
    /// `None` when `extract` is not this source's kind; `Err` when it is and
    /// the file does not give it.
    fn extract(&self, path: &Path, extract: &str) -> Option<Result<Extracted, String>>;
}

/// Run dirs of a state dir (module table).
pub trait AttemptSource {
    /// Every run dir (`<run id>/` and `keep-<run id>/`) with a readable
    /// `report.json`; unreadable ones as problems.
    fn runs(&self, state: &str) -> (Vec<RunAttempt>, Vec<String>);
    /// `holdout-reads.jsonl`, line by line; unreadable lines as problems.
    fn holdout_reads(&self, state: &str) -> (Vec<HoldoutRead>, Vec<String>);
}
