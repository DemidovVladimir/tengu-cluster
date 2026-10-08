//! Freeze a run dir (roadmap O4 "assumptions frozen before later outcomes
//! are observed"): the cycle's files become a sealed record. The cycle dir
//! is frozen in place — it already sits in the private SOE state — instead
//! of a copy in an evidence vault; the manifest format is the vault's
//! (`application::evidence::manifest_bytes`).
//!
//! | Use case | Rule |
//! |---|---|
//! | [`freeze`] | every file of the run dir (`CycleStore::files`) hashed → `MANIFEST.json` (`{path, sha256, bytes}` sorted by path, pretty JSON + `\n`) → `CycleStore::freeze` (read-only); returns the manifest's sha256 |
//! | [`verify`] | the manifest re-read and every file re-hashed: `MATCH` · `MISMATCH` · `ABSENT` (listed, gone) · `EXTRA` (present, not listed); no manifest ⇒ not frozen |
//! | [`decision_sha256`] | canonical sha256 of `{file: sha256}` over every file but `ops.json` (latencies are measurement, not decision): two runs of the same inputs give the same value |

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::application::evidence::manifest_bytes;
use crate::domain::canonical::{canonical_sha256, sha256_hex};
use crate::domain::evidence::ManifestEntry;
use crate::ports::soe::{CycleStore, RunDir, MANIFEST};

/// The file whose bytes measure the run, never decide it.
pub(crate) const OPS: &str = "ops.json";

fn sha256_bytes(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}

/// One manifest entry per file of `dir`, sorted by path.
pub(crate) fn entries(store: &dyn CycleStore, dir: &RunDir) -> Result<Vec<ManifestEntry>> {
    let mut out = Vec::new();
    for name in store.files(dir)? {
        let bytes = store
            .read(dir, &name)?
            .with_context(|| format!("{dir}/{name}: listed, not readable"))?;
        out.push(ManifestEntry {
            path: name,
            sha256: sha256_bytes(&bytes),
            bytes: bytes.len() as u64,
        });
    }
    Ok(out)
}

/// Module table: seal `dir`; the manifest's sha256.
pub(crate) fn freeze(store: &dyn CycleStore, dir: &RunDir) -> Result<String> {
    let manifest = manifest_bytes(&entries(store, dir)?)?;
    store.freeze(dir, &manifest)?;
    Ok(sha256_hex(
        std::str::from_utf8(&manifest).context("MANIFEST.json is not UTF-8")?,
    ))
}

/// One file's state against the manifest (module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum FileCheck {
    Match,
    Mismatch,
    Absent,
    Extra,
}

/// [`verify`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Verified {
    pub run: String,
    /// `None`: no `MANIFEST.json` — the run was never frozen.
    pub manifest_sha256: Option<String>,
    pub files: Vec<(String, FileCheck)>,
}

impl Verified {
    pub(crate) fn ok(&self) -> bool {
        self.manifest_sha256.is_some() && self.files.iter().all(|(_, c)| *c == FileCheck::Match)
    }
}

/// Module table: `dir` re-hashed against its manifest.
pub(crate) fn verify(store: &dyn CycleStore, dir: &RunDir) -> Result<Verified> {
    let Some(bytes) = store.read(dir, MANIFEST)? else {
        return Ok(Verified {
            run: dir.to_string(),
            manifest_sha256: None,
            files: Vec::new(),
        });
    };
    let listed: Vec<ManifestEntry> =
        serde_json::from_slice(&bytes).with_context(|| format!("{dir}/{MANIFEST}"))?;
    let now: BTreeMap<String, ManifestEntry> = entries(store, dir)?
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect();
    let mut files: Vec<(String, FileCheck)> = listed
        .iter()
        .map(|e| {
            let check = match now.get(&e.path) {
                None => FileCheck::Absent,
                Some(n) if n.sha256 == e.sha256 && n.bytes == e.bytes => FileCheck::Match,
                Some(_) => FileCheck::Mismatch,
            };
            (e.path.clone(), check)
        })
        .collect();
    for path in now.keys() {
        if !listed.iter().any(|e| &e.path == path) {
            files.push((path.clone(), FileCheck::Extra));
        }
    }
    files.sort();
    Ok(Verified {
        run: dir.to_string(),
        manifest_sha256: Some(sha256_hex(
            std::str::from_utf8(&bytes).context("MANIFEST.json is not UTF-8")?,
        )),
        files,
    })
}

/// Module table: the identity of what `dir` decided.
pub(crate) fn decision_sha256(store: &dyn CycleStore, dir: &RunDir) -> Result<String> {
    let files: BTreeMap<String, String> = entries(store, dir)?
        .into_iter()
        .filter(|e| e.path != OPS)
        .map(|e| (e.path, e.sha256))
        .collect();
    Ok(canonical_sha256(&serde_json::to_value(files)?))
}
