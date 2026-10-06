//! The lineage registry loader (`docs/lineage-2026-10-06.md` § 1): the
//! records under `lineage/` read into the pure `Registry`, and the
//! `config:` / `spec:` pins recomputed from the repo's sandbox configs.
//!
//! | Loader ([`load_registry`]) | Rule |
//! |---|---|
//! | dirs | `families/ variants/ experiments/ episodes/ incidents/ capabilities/ generations/ evidence/` — each optional; another dir is refused |
//! | files | `<id>.toml` per record, the file stem = its `id`; `locks.toml` at the root; `*.md` and dotfiles skipped; another file refused |
//! | parse | every table `deny_unknown_fields`; each error names the file |
//! | digest | `pins::toml_digest` of each record file (what `locks.toml` pins) |

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use super::paths;
use crate::domain::lineage::pins::{config_pin, spec_pin, toml_digest};
use crate::domain::lineage::value::{PinTarget, RecordKind};
use crate::domain::lineage::Registry;

/// The lock file at the registry root.
pub const LOCKS_FILE: &str = "locks.toml";

/// `<registry>/<kind dir>/<id>.toml`.
pub fn record_path(dir: &Path, kind: RecordKind, id: &str) -> PathBuf {
    dir.join(kind.dir()).join(format!("{id}.toml"))
}

/// The repo root of a registry dir: its parent (`repo:` locators, the
/// sandboxes `config:` / `spec:` pins read).
pub fn repo_root(registry_dir: &Path) -> PathBuf {
    let abs = paths::absolute_path(registry_dir);
    abs.parent().map_or(abs.clone(), Path::to_path_buf)
}

/// Skipped in the registry tree: docs and dotfiles.
fn skipped(name: &str) -> bool {
    name.starts_with('.') || name.ends_with(".md")
}

/// The `.toml` files of `dir`, sorted, as `(path, stem)`; other entries are
/// errors (module table).
fn toml_files(dir: &Path, errors: &mut Vec<String>) -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if skipped(&name) {
            continue;
        }
        match name.strip_suffix(".toml") {
            Some(stem) if path.is_file() => out.push((path, stem.to_string())),
            _ => errors.push(format!(
                "{}: not a record — <id>.toml files only",
                path.display()
            )),
        }
    }
    out.sort();
    out
}

/// One record file parsed, its id checked against the stem, its digest.
fn parse<T: DeserializeOwned>(
    path: &Path,
    stem: &str,
    id_of: impl Fn(&T) -> &str,
    errors: &mut Vec<String>,
) -> Option<(T, String)> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            errors.push(format!("{}: {e}", path.display()));
            return None;
        }
    };
    let record: T = match toml::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            errors.push(format!("{}: {}", path.display(), e.to_string().trim_end()));
            return None;
        }
    };
    let id = id_of(&record);
    if id != stem {
        errors.push(format!(
            "{}: id `{id}` differs from the file stem `{stem}` — the file is <id>.toml",
            path.display()
        ));
        return None;
    }
    match toml_digest(&text) {
        Ok(d) => Some((record, d)),
        Err(e) => {
            errors.push(format!("{}: {e}", path.display()));
            None
        }
    }
}

/// Module table: every record under `dir`; `Err` lists every problem, each
/// naming its file.
pub fn load_registry(dir: &Path) -> Result<Registry, Vec<String>> {
    if !dir.is_dir() {
        return Err(vec![format!("{}: no registry directory", dir.display())]);
    }
    let mut errors = Vec::new();
    let mut reg = Registry::default();
    let known: Vec<&str> = RecordKind::ALL.iter().map(|k| k.dir()).collect();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            if skipped(&name) || name == LOCKS_FILE {
                continue;
            }
            if !(path.is_dir() && known.contains(&name.as_str())) {
                errors.push(format!(
                    "{}: not a registry entry — the dirs are {} and the file {LOCKS_FILE}",
                    path.display(),
                    known.join(", ")
                ));
            }
        }
    }
    macro_rules! load {
        ($kind:expr, $map:ident, $ty:ty) => {
            for (path, stem) in toml_files(&dir.join($kind.dir()), &mut errors) {
                if let Some((r, digest)) =
                    parse::<$ty>(&path, &stem, |r: &$ty| r.id.as_str(), &mut errors)
                {
                    reg.digests.insert(($kind, r.id.clone()), digest);
                    reg.$map.insert(r.id.clone(), r);
                }
            }
        };
    }
    use crate::domain::lineage::{
        capability::Capability, episode::Episode, experiment::Experiment, family::Family,
        generation::Generation, incident::Incident, variant::Variant,
    };
    load!(RecordKind::Family, families, Family);
    load!(RecordKind::Variant, variants, Variant);
    load!(RecordKind::Experiment, experiments, Experiment);
    load!(RecordKind::Episode, episodes, Episode);
    load!(RecordKind::Incident, incidents, Incident);
    load!(RecordKind::Capability, capabilities, Capability);
    load!(RecordKind::Generation, generations, Generation);
    load!(
        RecordKind::Evidence,
        evidence,
        crate::domain::evidence::EvidenceRecord
    );
    let locks = dir.join(LOCKS_FILE);
    if locks.exists() {
        match std::fs::read_to_string(&locks).map_err(|e| e.to_string()) {
            Ok(text) => match toml::from_str(&text) {
                Ok(l) => reg.locks = l,
                Err(e) => errors.push(format!("{}: {}", locks.display(), e.to_string().trim_end())),
            },
            Err(e) => errors.push(format!("{}: {e}", locks.display())),
        }
    }
    if errors.is_empty() {
        Ok(reg)
    } else {
        Err(errors)
    }
}

/// `<repo>/sandboxes/<sandbox>/config.toml` of a registry dir.
pub fn sandbox_config_path(registry_dir: &Path, sandbox: &str) -> PathBuf {
    repo_root(registry_dir)
        .join("sandboxes")
        .join(sandbox)
        .join("config.toml")
}

/// A `config:` / `spec:` pin recomputed from the sandbox config under the
/// registry's repo; `None` for the other targets.
pub fn sandbox_pin(registry_dir: &Path, target: &PinTarget) -> Option<Result<String, String>> {
    let sandbox = match target {
        PinTarget::Config { sandbox, .. } | PinTarget::Spec { sandbox, .. } => sandbox,
        _ => return None,
    };
    let file = sandbox_config_path(registry_dir, sandbox);
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) => return Some(Err(format!("{}: {e}", file.display()))),
    };
    Some(match target {
        PinTarget::Config { path, .. } => config_pin(&text, path),
        PinTarget::Spec { strategy, .. } => spec_pin(&text, strategy),
        _ => unreachable!("matched above"),
    })
}

#[cfg(test)]
#[path = "lineage_tests.rs"]
mod tests;
