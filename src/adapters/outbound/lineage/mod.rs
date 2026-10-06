//! Lineage adapters — the files behind `ports/lineage.rs`
//! (`docs/lineage-2026-10-06.md` § 1–2). Read-only: nothing here writes a
//! registry, a vault, a run dir or a state file.
//!
//! | File | Implements | Reads |
//! |---|---|---|
//! | `probe.rs` | `ContractProbe` ([`RepoProbe`]) | `tool_schema:` the catalog's input schemas; `config:` / `spec:` `<repo>/sandboxes/<s>/config.toml`; `skill:` `<repo>/skills/<name>/SKILL.md`; `repo:` the file |
//! | `resolver.rs` | `EvidenceResolver` ([`FsResolver`]) | `repo:` · `run:` (run dir, `keep-` dir, else its vault copy) · `vault:` (+ the record's item / `MANIFEST.json` sha256) · `state:` (mutable) · `git:` (`git cat-file -e` when asked) |
//! | `runs.rs` | `AttemptSource` ([`RunDirs`]) · `ResultSource` ([`ReportJson`]: `arm:<name>`, `arm:<name>/in_sample`, `arm:<name>/holdout`, `gate`) | `<TENGU_HOME>/state/<state>/backtests/*/report.json`, `holdout-reads.jsonl` |
//! | `ledger.rs` | `ResultSource` ([`ledger::LedgerGrade`]: `ledger:<account>`) | a paper `ledger.db` (vault copy), graded by `domain::xm::grade` |
//!
//! Result sources are a list ([`result_sources`]): a new `extract` kind
//! is one more `ResultSource` there.

pub(crate) mod ledger;
pub(crate) mod probe;
pub(crate) mod resolver;
pub(crate) mod runs;

pub(crate) use probe::RepoProbe;
pub(crate) use resolver::FsResolver;
pub(crate) use runs::{ReportJson, RunDirs};

use crate::ports::lineage::ResultSource;

/// Every result source, in the order `verify` asks them.
pub(crate) fn result_sources() -> Vec<Box<dyn ResultSource>> {
    vec![Box::new(ReportJson), Box::new(ledger::LedgerGrade)]
}
