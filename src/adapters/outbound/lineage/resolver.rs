//! `EvidenceResolver` on the filesystem (`docs/lineage-2026-10-06.md` § 1
//! locators). Read-only; hashes are sha256 of the bytes, a dir's is the tree
//! hash (`domain/evidence.rs::tree_hash`).
//!
//! | Locator | Resolves to | Recorded sha256 |
//! |---|---|---|
//! | `repo:<path>` | `<repo>/<path>` | the ref's |
//! | `run:<state>/<run id>[/<file>]` | `<TENGU_HOME>/state/<state>/backtests/<run id>/<file \| report.json>`, else `keep-<run id>/…`, else the vault copy an evidence record's item holds (`<state>/backtests/<run id>` or `…/keep-<run id>`) | the vault's when it is the copy |
//! | `vault:<snapshot>/<path>` | `<TENGU_HOME>/state/evidence/<snapshot>/<path>` | the evidence record item with that vault + path, else the vault's `MANIFEST.json` entry (a dir: the tree hash of its entries) |
//! | `state:<state>/<path>` | `<TENGU_HOME>/state/<state>/<path>` — mutable | — |
//! | `git:<commit>` | `git -C <repo> cat-file -e <commit>^{commit}` when `check_git`, else nothing to open | — |
//! | `record:` · `url:` · `UNKNOWN` | nothing to open (`record:` is checked by `Registry::validate`) | — |

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::domain::evidence::{tree_hash, EvidenceRecord, ItemKind, ManifestEntry};
use crate::domain::lineage::pins::bytes_sha256;
use crate::domain::lineage::value::Locator;
use crate::ports::lineage::{EvidenceResolver, Resolution};

/// Module table.
pub(crate) struct FsResolver {
    pub repo: PathBuf,
    pub tengu_home: PathBuf,
    /// The registry's evidence records (vault items + their sha256).
    pub evidence: Vec<EvidenceRecord>,
    /// Ask git whether a `git:` commit exists (`verify --evidence`).
    pub check_git: bool,
}

/// Every regular file under `dir`, `/`-separated relative paths, sorted.
fn files_under(dir: &Path) -> Vec<(String, PathBuf)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else {
                continue;
            };
            if ft.is_dir() {
                walk(root, &p, out);
            } else if ft.is_file() {
                if let Ok(rel) = p.strip_prefix(root) {
                    let rel = rel
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().to_string())
                        .collect::<Vec<_>>()
                        .join("/");
                    out.push((rel, p));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    out
}

/// sha256 of a file's bytes, or a dir's tree hash.
pub(crate) fn path_sha256(path: &Path) -> Result<String, String> {
    if path.is_dir() {
        let mut entries = Vec::new();
        for (rel, p) in files_under(path) {
            let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            entries.push(ManifestEntry {
                path: format!("d/{rel}"),
                sha256: bytes_sha256(&bytes),
                bytes: bytes.len() as u64,
            });
        }
        return Ok(tree_hash("d", &entries));
    }
    std::fs::read(path)
        .map(|b| bytes_sha256(&b))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// The entries of a vault's `MANIFEST.json`: an array of `{path, sha256,
/// bytes}`, or an object holding one (`files` / `entries` / `items`).
fn manifest_entries(vault: &Path) -> Vec<ManifestEntry> {
    let Ok(text) = std::fs::read_to_string(vault.join("MANIFEST.json")) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let list = match &v {
        Value::Array(_) => Some(&v),
        Value::Object(o) => ["files", "entries", "items"]
            .iter()
            .find_map(|k| o.get(*k).filter(|x| x.is_array())),
        _ => None,
    };
    list.and_then(|l| serde_json::from_value::<Vec<ManifestEntry>>(l.clone()).ok())
        .unwrap_or_default()
}

impl FsResolver {
    fn state_dir(&self, state: &str) -> PathBuf {
        self.tengu_home.join("state").join(state)
    }

    fn vault_dir(&self, snapshot: &str) -> PathBuf {
        self.tengu_home
            .join("state")
            .join("evidence")
            .join(snapshot)
    }

    fn present(path: PathBuf, recorded: Option<String>, mutable: bool) -> Resolution {
        if !path.exists() {
            return Resolution::Missing(format!("{} does not exist", path.display()));
        }
        match path_sha256(&path) {
            Ok(sha) => Resolution::Present {
                path,
                sha256: Some(sha),
                recorded,
                mutable,
            },
            Err(e) => Resolution::Missing(format!("unreadable: {e}")),
        }
    }

    /// What the vault records for `path` (module table).
    fn vault_recorded(&self, snapshot: &str, path: &str) -> Option<String> {
        let item = self
            .evidence
            .iter()
            .filter(|r| r.vault == snapshot)
            .flat_map(|r| &r.items)
            .find(|i| i.path == path);
        if let Some(sha) = item.and_then(|i| i.sha256.clone()) {
            return Some(sha);
        }
        let entries = manifest_entries(&self.vault_dir(snapshot));
        if let Some(e) = entries.iter().find(|e| e.path == path) {
            return Some(e.sha256.clone());
        }
        let prefix = format!("{path}/");
        entries
            .iter()
            .any(|e| e.path.starts_with(&prefix))
            .then(|| tree_hash(path, &entries))
    }

    fn vault(&self, snapshot: &str, path: &str) -> Resolution {
        let full = self.vault_dir(snapshot).join(path);
        Self::present(full, self.vault_recorded(snapshot, path), false)
    }

    /// The vault copy of a run dir an evidence record holds.
    fn run_in_vault(&self, state: &str, run_id: &str, file: &str) -> Option<Resolution> {
        let tails = [
            format!("{state}/backtests/{run_id}"),
            format!("{state}/backtests/keep-{run_id}"),
        ];
        for r in &self.evidence {
            for it in &r.items {
                let hit = tails
                    .iter()
                    .any(|t| it.path == *t || it.path.ends_with(&format!("/{t}")));
                if hit && it.kind == ItemKind::Dir {
                    return Some(self.vault(&r.vault, &format!("{}/{file}", it.path)));
                }
            }
        }
        None
    }
}

impl EvidenceResolver for FsResolver {
    fn resolve(&self, locator: &Locator) -> Resolution {
        match locator {
            Locator::Repo(p) => Self::present(self.repo.join(p), None, false),
            Locator::Run {
                state,
                run_id,
                file,
            } => {
                let file = file.as_deref().unwrap_or("report.json");
                let base = self.state_dir(state).join("backtests");
                for dir in [run_id.clone(), format!("keep-{run_id}")] {
                    let p = base.join(&dir).join(file);
                    if p.exists() {
                        return Self::present(p, None, false);
                    }
                }
                self.run_in_vault(state, run_id, file).unwrap_or_else(|| {
                    Resolution::Missing(format!(
                        "no {}/{run_id} or keep-{run_id} holding {file}, and no vault copy",
                        base.display()
                    ))
                })
            }
            Locator::Vault { snapshot, path } => self.vault(snapshot, path),
            Locator::State { state, path } => {
                Self::present(self.state_dir(state).join(path), None, true)
            }
            Locator::Git(commit) if self.check_git => {
                let ok = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&self.repo)
                    .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                if ok {
                    Resolution::Present {
                        path: self.repo.clone(),
                        sha256: None,
                        recorded: None,
                        mutable: false,
                    }
                } else {
                    Resolution::Missing(format!(
                        "commit {commit} is not in the repo at {}",
                        self.repo.display()
                    ))
                }
            }
            Locator::Git(_) | Locator::Record { .. } | Locator::Url(_) | Locator::Unknown => {
                Resolution::NotAFile
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::lineage::load_registry;
    use crate::domain::evidence::{EvidenceClass, EvidenceItem, Provenance};

    const LEDGER: &str = "f20d3793f2c4430cfa05260f0e01a75b82822cc1644e8a49c6a81e79e0b8c38d";

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lineage")
    }

    fn resolver(with_records: bool) -> FsResolver {
        let reg = load_registry(&root().join("registry")).unwrap();
        FsResolver {
            repo: root(),
            tengu_home: root().join("home"),
            evidence: if with_records {
                reg.evidence.values().cloned().collect()
            } else {
                Vec::new()
            },
            check_git: false,
        }
    }

    fn at(r: &FsResolver, s: &str) -> Resolution {
        r.resolve(&s.parse().unwrap())
    }

    #[test]
    fn locators_resolve_with_what_the_vault_records() {
        for with_records in [true, false] {
            let r = resolver(with_records);
            match at(&r, "vault:w1-fixture/xmarket-weekend/ledger.db") {
                Resolution::Present {
                    sha256,
                    recorded,
                    mutable,
                    ..
                } => {
                    assert_eq!(sha256.as_deref(), Some(LEDGER));
                    // The record's item, else MANIFEST.json: the same hash.
                    assert_eq!(recorded.as_deref(), Some(LEDGER));
                    assert!(!mutable);
                }
                other => panic!("{other:?}"),
            }
            // A dir: its tree hash, and the manifest's tree hash of it.
            match at(&r, "vault:w1-fixture/xmarket-weekend") {
                Resolution::Present {
                    sha256, recorded, ..
                } => assert_eq!(sha256, recorded),
                other => panic!("{other:?}"),
            }
        }
        let r = resolver(true);
        match at(&r, "run:xlab/20260930T090000Z-rule_w") {
            Resolution::Present { path, .. } => {
                assert!(path.ends_with("keep-20260930T090000Z-rule_w/report.json"))
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            at(&r, "run:xlab/20990101T000000Z-x"),
            Resolution::Missing(_)
        ));
        assert!(matches!(
            at(&r, "state:xlab/market.db"),
            Resolution::Present { mutable: true, .. }
        ));
        match at(&r, "repo:docs/study.md") {
            Resolution::Present { sha256, .. } => assert_eq!(
                sha256.as_deref(),
                Some("91d4fe312748426f951dcbe714ff2d98a14223015ffe1400879dee66419cfa05")
            ),
            other => panic!("{other:?}"),
        }
        for s in [
            "url:https://example.com",
            "record:family/rule_w",
            "git:c80d5beb724ff7eaf2fc61a4928ff15aa9a7cd16",
            "UNKNOWN",
        ] {
            assert_eq!(at(&r, s), Resolution::NotAFile, "{s}");
        }
    }

    #[test]
    fn a_vault_copy_stands_in_for_a_pruned_run_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp
            .path()
            .join("state/evidence/v1/xlab/backtests/20261001T000000Z-x");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("report.json"), "{}").unwrap();
        let item = EvidenceItem {
            path: "xlab/backtests/20261001T000000Z-x".into(),
            source: "~/.tengu/state/xlab/backtests/20261001T000000Z-x".into(),
            kind: ItemKind::Dir,
            sha256: None,
            files: None,
            bytes: None,
            provenance: Provenance::Derived,
            class: EvidenceClass::Development,
            role: "run-dir".into(),
            note: None,
        };
        let r = FsResolver {
            repo: tmp.path().to_path_buf(),
            tengu_home: tmp.path().to_path_buf(),
            evidence: vec![EvidenceRecord {
                id: "ev".into(),
                title: "t".into(),
                notes: None,
                vault: "v1".into(),
                captured_at: None,
                manifest_sha256: None,
                experiment: None,
                items: vec![item],
            }],
            check_git: false,
        };
        match at(&r, "run:xlab/20261001T000000Z-x") {
            Resolution::Present { path, .. } => {
                assert!(path.starts_with(tmp.path().join("state/evidence/v1")))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn git_commits_are_checked_only_when_asked() {
        let mut r = resolver(false);
        r.repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        r.check_git = true;
        // The commit the lineage domain landed on (`domain/evidence.rs`).
        assert!(matches!(
            at(&r, "git:c80d5beb724ff7eaf2fc61a4928ff15aa9a7cd16"),
            Resolution::Present { .. }
        ));
        assert!(matches!(
            at(&r, &format!("git:{}", "0".repeat(40))),
            Resolution::Missing(_)
        ));
    }
}
