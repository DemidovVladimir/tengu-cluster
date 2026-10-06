//! Evidence — what an experiment's figures rest on (`docs/lineage-2026-10-06.md`
//! § 1, § 3): the class of a piece of evidence, where it came from, and the
//! snapshot record (`lineage/evidence/<id>.toml`) that pins a read-only vault
//! copy (`<TENGU_HOME>/state/evidence/<vault>/`) by sha256. Pure: the copy,
//! the hashing of files and the vault live in `adapters/outbound/evidence/`.
//!
//! | Type | Values |
//! |---|---|
//! | [`EvidenceClass`] | `DEVELOPMENT` · `HOLDOUT` · `FORWARD_PAPER` · `LIVE_MICRO` · `LIVE_PRODUCTION` · `NONE` (docs, code, config) — never mixed (handoff § 26) |
//! | [`Provenance`] | `LIVE_RECORDED` · `BACKFILLED` · `MISSING` · `NOT_APPLICABLE` · `DERIVED` (recomputable by tengu from other evidence) · `REPORTED` (a figure in a doc whose raw inputs are gone) |
//! | [`EvidenceRecord`] | `id`, `title`, `vault`, `captured_at` + `manifest_sha256` (set by the snapshot, absent in a plan), `experiment?`, `[[items]]` |
//! | [`EvidenceItem`] | `path` (vault-relative), `source` (`~/…` or absolute), `kind` `FILE` \| `DIR`, `sha256` / `files` / `bytes` (set by the snapshot), `provenance`, `class`, `role`, `note?` |
//!
//! | Rule | Value |
//! |---|---|
//! | Item path | relative, `/`-separated, no empty / `.` / `..` segment, unique — also ignoring ASCII case (APFS); no item inside another; never `MANIFEST.json` (any case) as its first segment |
//! | File hash | sha256 of the bytes, 64 lowercase hex — never shortened |
//! | Dir hash ([`tree_hash`]) | sha256 of the lines `"<sha256>  <relpath>\n"` of every regular file under the dir, sorted bytewise by relpath (`/`-separated) — what `shasum -a 256` prints, sorted |
//! | Captured | `captured_at`, `manifest_sha256` and every item's `sha256` / `files` / `bytes` are all set, or none is (a plan) |

// Consumers land with `tengu evidence` and `tengu lineage` (lineage P0–P5).
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::domain::canonical::sha256_hex;

/// The vault's own manifest at its root: no item may take its name.
pub const MANIFEST_NAME: &str = "MANIFEST.json";

/// Where a figure's evidence sits in the development → forward ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EvidenceClass {
    Development,
    Holdout,
    ForwardPaper,
    LiveMicro,
    LiveProduction,
    /// Docs, code, config: not a market observation.
    None,
}

/// How a piece of evidence came to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Provenance {
    /// Recorded live while it happened (recorder day files, ledger fills).
    LiveRecorded,
    /// Fetched later from an archive or API (`market.db`).
    Backfilled,
    /// Never recorded and not recoverable.
    Missing,
    NotApplicable,
    /// Computed by tengu from other evidence; rerunnable.
    Derived,
    /// A figure stated in a document whose raw inputs are gone.
    Reported,
}

/// One snapshot item: a file or a directory tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ItemKind {
    File,
    Dir,
}

/// One item of an evidence snapshot (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceItem {
    /// Vault-relative path (`xmarket-weekend/ledger.db`).
    pub path: String,
    /// Where it was copied from: `~/…` or absolute.
    pub source: String,
    pub kind: ItemKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    pub provenance: Provenance,
    pub class: EvidenceClass,
    /// Short label: `ledger`, `recorder`, `log`, `run-dir`, `prereg`, `binary`, …
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `lineage/evidence/<id>.toml` (module table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRecord {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// The vault dir name under `<TENGU_HOME>/state/evidence/`.
    pub vault: String,
    /// RFC 3339 UTC instant the snapshot was taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    /// sha256 of the vault's `MANIFEST.json` bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_sha256: Option<String>,
    /// The experiment this evidence belongs to, when one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment: Option<String>,
    pub items: Vec<EvidenceItem>,
}

/// One file of a vault's `MANIFEST.json`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    /// Vault-relative, `/`-separated.
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

/// 64 lowercase hex chars.
pub fn valid_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// A vault-relative path (module table).
pub fn valid_item_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// The tree hash of `entries` relative to their dir (module table): `relpath`
/// = each entry's path with `dir/` removed; entries outside `dir` are ignored.
pub fn tree_hash(dir: &str, entries: &[ManifestEntry]) -> String {
    let prefix = format!("{dir}/");
    let mut lines: Vec<(&str, &str)> = entries
        .iter()
        .filter_map(|e| {
            e.path
                .strip_prefix(&prefix)
                .map(|rel| (rel, e.sha256.as_str()))
        })
        .collect();
    lines.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut text = String::new();
    for (rel, sha) in lines {
        text.push_str(sha);
        text.push_str("  ");
        text.push_str(rel);
        text.push('\n');
    }
    sha256_hex(&text)
}

impl EvidenceRecord {
    /// Every captured field set (module table: Captured).
    pub fn is_captured(&self) -> bool {
        self.captured_at.is_some()
    }

    /// Record-local problems, one line each naming the item.
    pub fn validation_errors(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.vault.is_empty() || !valid_item_path(&self.vault) || self.vault.contains('/') {
            errs.push(format!("vault `{}`: one path segment", self.vault));
        }
        if self.items.is_empty() {
            errs.push("items: none".into());
        }
        let mut seen = BTreeSet::new();
        let mut folded: BTreeMap<String, &str> = BTreeMap::new();
        for item in &self.items {
            if !valid_item_path(&item.path) {
                errs.push(format!("item `{}`: not a vault-relative path", item.path));
            }
            if item
                .path
                .split('/')
                .next()
                .is_some_and(|first| first.eq_ignore_ascii_case(MANIFEST_NAME))
            {
                errs.push(format!(
                    "item `{}`: {MANIFEST_NAME} is the vault's own manifest",
                    item.path
                ));
            }
            if !seen.insert(item.path.as_str()) {
                errs.push(format!("item `{}`: listed twice", item.path));
            } else if let Some(other) = folded.insert(item.path.to_lowercase(), &item.path) {
                errs.push(format!(
                    "item `{}` and item `{other}` differ only in case — one file on a \
                     case-insensitive disk",
                    item.path
                ));
            }
            if item.source.is_empty() {
                errs.push(format!("item `{}`: no source", item.path));
            }
        }
        for a in &self.items {
            for b in &self.items {
                let (al, bl) = (a.path.to_lowercase(), b.path.to_lowercase());
                if al != bl && bl.starts_with(&format!("{al}/")) {
                    errs.push(format!("item `{}` lies inside item `{}`", b.path, a.path));
                }
            }
        }
        let captured = self.captured_at.is_some();
        let complete = |i: &EvidenceItem| {
            i.sha256.as_deref().is_some_and(valid_sha256) && i.files.is_some() && i.bytes.is_some()
        };
        let any_hash = self.manifest_sha256.is_some()
            || self
                .items
                .iter()
                .any(|i| i.sha256.is_some() || i.files.is_some() || i.bytes.is_some());
        if captured {
            if !self.manifest_sha256.as_deref().is_some_and(valid_sha256) {
                errs.push("manifest_sha256: missing or not 64 hex chars".into());
            }
            for item in self.items.iter().filter(|i| !complete(i)) {
                errs.push(format!(
                    "item `{}`: captured record without sha256 / files / bytes",
                    item.path
                ));
            }
        } else if any_hash {
            errs.push("a plan (no captured_at) carries hashes".into());
        }
        errs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, sha: &str) -> ManifestEntry {
        ManifestEntry {
            path: path.into(),
            sha256: sha.into(),
            bytes: 1,
        }
    }

    #[test]
    fn the_tree_hash_is_the_sorted_shasum_listing_of_the_dir() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let entries = vec![
            entry("runs/x/report.json", &b),
            entry("runs/x/a.jsonl", &a),
            entry("other/y", &a),
        ];
        let expected = sha256_hex(&format!("{a}  x/a.jsonl\n{b}  x/report.json\n"));
        assert_eq!(tree_hash("runs", &entries), expected);
        let mut reversed = entries.clone();
        reversed.reverse();
        assert_eq!(tree_hash("runs", &reversed), expected);
        assert_eq!(tree_hash("none", &entries), sha256_hex(""));
    }

    #[test]
    fn item_paths_are_relative_and_never_climb() {
        assert!(valid_item_path("xmarket-weekend/ledger.db"));
        for bad in ["", "/abs", "a/../b", "./a", "a//b", "a\\b", "a/"] {
            assert!(!valid_item_path(bad), "{bad}");
        }
    }

    #[test]
    fn a_record_is_a_plan_or_fully_captured() {
        let plan: EvidenceRecord = toml::from_str(
            r#"
id = "w"
title = "t"
vault = "w"
[[items]]
path = "s/ledger.db"
source = "~/.tengu/state/s/ledger.db"
kind = "FILE"
provenance = "LIVE_RECORDED"
class = "FORWARD_PAPER"
role = "ledger"
"#,
        )
        .unwrap();
        assert!(plan.validation_errors().is_empty());
        let mut half = plan.clone();
        half.captured_at = Some("2026-10-06T10:00:00Z".into());
        assert!(half
            .validation_errors()
            .iter()
            .any(|e| e.contains("manifest_sha256")));
        let mut nested = plan.clone();
        nested.items.push(EvidenceItem {
            path: "s".into(),
            ..plan.items[0].clone()
        });
        assert!(nested
            .validation_errors()
            .iter()
            .any(|e| e.contains("lies inside")));
        let unknown = toml::from_str::<EvidenceRecord>(
            "id = \"w\"\ntitle = \"t\"\nvault = \"w\"\nitems = []\nextra = 1\n",
        );
        assert!(unknown.is_err());
    }

    /// Review #19: an item named like the manifest, or two items one
    /// case-insensitive disk folds together, would leave a half-made vault
    /// that blocks every retry — refused before anything is copied.
    #[test]
    fn manifest_named_and_case_twin_items_are_refused() {
        let plan: EvidenceRecord = toml::from_str(
            r#"
id = "w"
title = "t"
vault = "w"
[[items]]
path = "X/a.db"
source = "/s/a.db"
kind = "FILE"
provenance = "LIVE_RECORDED"
class = "FORWARD_PAPER"
role = "ledger"
"#,
        )
        .unwrap();
        assert!(plan.validation_errors().is_empty());
        for bad in ["MANIFEST.json", "manifest.JSON", "Manifest.json/x"] {
            let mut r = plan.clone();
            r.items[0].path = bad.into();
            assert!(
                r.validation_errors().iter().any(|e| e.contains("manifest")),
                "{bad}: {:?}",
                r.validation_errors()
            );
        }
        let mut twins = plan.clone();
        twins.items.push(EvidenceItem {
            path: "x/a.db".into(),
            ..plan.items[0].clone()
        });
        assert!(twins
            .validation_errors()
            .iter()
            .any(|e| e.contains("differ only in case")));
        let mut nested = plan.clone();
        nested.items.push(EvidenceItem {
            path: "x".into(),
            ..plan.items[0].clone()
        });
        assert!(nested
            .validation_errors()
            .iter()
            .any(|e| e.contains("lies inside")));
    }
}
