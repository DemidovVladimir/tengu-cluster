//! The lineage registry loader and `[generation]` — a sandbox bound to one
//! generation (`docs/lineage-2026-10-06.md` § 1, § 4; roadmap P5). Hard
//! enforcement: `Config::load` (every surface) refuses a config that could
//! reach a capability outside its generation.
//!
//! | Loader ([`load_registry`]) | Rule |
//! |---|---|
//! | dirs | `families/ variants/ experiments/ episodes/ incidents/ capabilities/ generations/ evidence/` — each optional; another dir is refused |
//! | files | `<id>.toml` per record, the file stem = its `id`; `locks.toml` at the root; `*.md` and dotfiles skipped; another file refused |
//! | parse | every table `deny_unknown_fields`; each error names the file |
//! | digest | `pins::toml_digest` of each record file (what `locks.toml` pins) |
//!
//! ```toml
//! [generation]               # in sandboxes/<name>/config.toml
//! id = "W1"
//! registry = "../../lineage" # relative to this file
//! ```
//!
//! | Load rule ([`binding_errors`]; a violation fails `Config::load`) | The error names |
//! |---|---|
//! | the registry loads; `generations/<id>.toml` exists; it lists this sandbox (`paths::sandbox_of_config_file`) in `sandboxes` | generation, sandbox |
//! | no Error finding of the registry on this generation (`frozen_manifest_changed`, `capability_version_missing`, its references) or any `binding_conflict` | the finding |
//! | every tool an agent lists (`tools`, `workspace_tools`), every `[feeds.*]` tool and `[decision_loops.*]` action tool passes `GenerationScope::tool_refusal`: a tool some capability binds must be bound by one of the generation's; an opt-in tool (`WORKSPACE_TOOLS`) no capability binds is refused | sandbox, agent / feed / loop + action, tool, capability, generation |
//! | every `[backtest.strategies.*]` kind passes `GenerationScope::kind_refusal` | sandbox, strategy, kind, capability, generation |
//! | a `FROZEN` generation's `config:` / `spec:` pins recompute equal — this sandbox's from the raw text just loaded, another's from `<repo>/sandboxes/<s>/config.toml` (repo = the registry's parent; `tool_schema:` pins: `tengu lineage verify --pins`) | generation, pin, both hashes |
//!
//! The resolved `GenerationScope` reaches tools as `SandboxSections::generation`
//! (`Config::sandbox_sections`): the backtest use case refuses a spec kind
//! outside it (`capability_unavailable`), `bootstrap/tools.rs` registers no
//! tool outside it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::paths;
use super::sections::DEFAULT_SANDBOX;
use super::Config;
use crate::domain::lineage::generation::{GenerationScope, GenerationStatus};
use crate::domain::lineage::pins::{config_pin, spec_pin, toml_digest};
use crate::domain::lineage::registry::label;
use crate::domain::lineage::value::{PinTarget, RecordKind};
use crate::domain::lineage::{Registry, Severity};

/// The lock file at the registry root.
pub const LOCKS_FILE: &str = "locks.toml";

/// `[generation]` (module example).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationBinding {
    pub id: String,
    /// The registry dir, relative to the config file's dir.
    pub registry: PathBuf,
}

impl GenerationBinding {
    /// The registry dir for the config file at `config_file`.
    pub fn registry_dir(&self, config_file: &Path) -> PathBuf {
        if self.registry.is_absolute() {
            return self.registry.clone();
        }
        config_file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&self.registry)
    }
}

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
/// errors (module table; also the SOE record dirs, `config/soe.rs`).
pub(crate) fn toml_files(dir: &Path, errors: &mut Vec<String>) -> Vec<(PathBuf, String)> {
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

/// One record file parsed, its id checked against the stem, its digest
/// (also `config/soe.rs`).
pub(crate) fn parse<T: DeserializeOwned>(
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

/// The `[generation]` of `<repo>/sandboxes/<sandbox>/config.toml` (raw
/// text) and the registry dir it resolves to; `Ok(None)` = none declared.
pub fn declared_binding(
    registry_dir: &Path,
    sandbox: &str,
) -> Result<Option<(GenerationBinding, PathBuf)>, String> {
    #[derive(Deserialize)]
    struct OnlyGeneration {
        generation: Option<GenerationBinding>,
    }
    let file = sandbox_config_path(registry_dir, sandbox);
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let parsed: OnlyGeneration =
        toml::from_str(&text).map_err(|e| format!("{}: {}", file.display(), e.message()))?;
    Ok(parsed.generation.map(|b| {
        let dir = b.registry_dir(&file);
        (b, dir)
    }))
}

/// `a` and `b` name the same directory (canonical paths; else absolute).
pub fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| paths::absolute_path(p));
    norm(a) == norm(b)
}

/// A `config:` / `spec:` pin recomputed from the sandbox config under the
/// registry's repo; `None` for the other targets.
pub fn sandbox_pin(registry_dir: &Path, target: &PinTarget) -> Option<Result<String, String>> {
    let sandbox = pin_sandbox(target)?;
    let file = sandbox_config_path(registry_dir, sandbox);
    Some(match std::fs::read_to_string(&file) {
        Ok(text) => pin_of_text(&text, target)?,
        Err(e) => Err(format!("{}: {e}", file.display())),
    })
}

/// The sandbox a `config:` / `spec:` pin reads; `None` for the other targets.
fn pin_sandbox(target: &PinTarget) -> Option<&str> {
    match target {
        PinTarget::Config { sandbox, .. } | PinTarget::Spec { sandbox, .. } => Some(sandbox),
        _ => None,
    }
}

/// A `config:` / `spec:` pin over a config text; `None` for the other targets.
fn pin_of_text(text: &str, target: &PinTarget) -> Option<Result<String, String>> {
    match target {
        PinTarget::Config { path, .. } => Some(config_pin(text, path)),
        PinTarget::Spec { strategy, .. } => Some(spec_pin(text, strategy)),
        _ => None,
    }
}

/// The module's load rules for `cfg` read from `path` (`text` = the raw
/// file text it was parsed from); the scope when the config binds a
/// generation that exists.
pub(crate) fn binding_errors(
    cfg: &Config,
    path: &Path,
    text: &str,
) -> (Vec<String>, Option<GenerationScope>) {
    let Some(binding) = &cfg.generation else {
        return (Vec::new(), None);
    };
    let gid = &binding.id;
    let at = format!("[generation] id = \"{gid}\"");
    let dir = binding.registry_dir(path);
    let reg = match load_registry(&dir) {
        Ok(r) => r,
        Err(es) => {
            return (
                es.into_iter()
                    .map(|e| format!("{at}: registry {}: {e}", dir.display()))
                    .collect(),
                None,
            )
        }
    };
    let Some(g) = reg.generations.get(gid) else {
        return (
            vec![format!(
                "{at}: no generations/{gid}.toml in the registry {}",
                dir.display()
            )],
            None,
        );
    };
    let mut errs = Vec::new();
    let sandbox = paths::sandbox_of_config_file(path);
    match &sandbox {
        None => errs.push(format!(
            "{at}: {} is no sandboxes/<name>/config.toml — a generation lists its sandboxes by name",
            path.display()
        )),
        Some(s) if !g.sandboxes.contains(s) => errs.push(format!(
            "{at}: generation `{gid}` does not list sandbox `{s}` (its sandboxes: {})",
            if g.sandboxes.is_empty() {
                "none".to_string()
            } else {
                g.sandboxes.join(", ")
            }
        )),
        Some(_) => {}
    }
    let mine = label(RecordKind::Generation, gid);
    for f in reg.validate() {
        if f.severity == Severity::Error && (f.record == mine || f.code == "binding_conflict") {
            errs.push(format!("{at}: {} {}: {}", f.code, f.record, f.message));
        }
    }
    let scope = match GenerationScope::of(&reg, gid) {
        Ok(s) => s,
        Err(e) => return (vec![format!("{at}: {e}")], None),
    };
    let s = sandbox.as_deref().unwrap_or(DEFAULT_SANDBOX);
    let mut agents: Vec<_> = cfg.agents.iter().collect();
    agents.sort_by(|a, b| a.0.cmp(b.0));
    for (aid, a) in agents {
        let tools: BTreeSet<&str> = a
            .tools
            .iter()
            .chain(&a.workspace_tools)
            .map(String::as_str)
            .collect();
        for t in tools {
            if let Some(why) = scope.tool_refusal(t) {
                errs.push(format!("sandbox `{s}` agent `{aid}`: {why}"));
            }
        }
    }
    for (name, feed) in &cfg.feeds {
        if let Some(why) = feed.tool.as_deref().and_then(|t| scope.tool_refusal(t)) {
            errs.push(format!("sandbox `{s}` feed `{name}`: {why}"));
        }
    }
    let mut loops: Vec<_> = cfg.decision_loops.iter().collect();
    loops.sort_by(|a, b| a.0.cmp(b.0));
    for (name, dl) in loops {
        for (an, action) in &dl.actions {
            if let Some(why) = action.tool.as_deref().and_then(|t| scope.tool_refusal(t)) {
                errs.push(format!("sandbox `{s}` loop `{name}` action `{an}`: {why}"));
            }
        }
    }
    if let Some(bt) = &cfg.backtest {
        for (name, spec) in &bt.strategies {
            let kind = spec
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<none>");
            if let Some(why) = scope.kind_refusal(kind) {
                errs.push(format!("sandbox `{s}` strategy `{name}`: {why}"));
            }
        }
    }
    if g.status == GenerationStatus::Frozen {
        for p in &g.pins {
            // This sandbox's pins: the text just loaded (no re-read, no other
            // copy); another sandbox's: its repo file.
            let now = if sandbox.is_some() && pin_sandbox(&p.target) == sandbox.as_deref() {
                pin_of_text(text, &p.target)
            } else {
                sandbox_pin(&dir, &p.target)
            };
            match now {
                None => {}
                Some(Ok(now)) if now == p.sha256 => {}
                Some(Ok(now)) => errs.push(format!(
                    "{at}: FROZEN generation `{gid}` pin `{}` drifted: pinned {}, now {now}",
                    p.target, p.sha256
                )),
                Some(Err(e)) => errs.push(format!(
                    "{at}: FROZEN generation `{gid}` pin `{}` does not resolve: {e}",
                    p.target
                )),
            }
        }
    }
    (errs, Some(scope))
}

#[cfg(test)]
#[path = "lineage_tests.rs"]
mod tests;
