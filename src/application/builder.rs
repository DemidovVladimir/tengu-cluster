//! Studio builder use cases (`docs/studio-builder-2026-10-10.md`): the page
//! state, autosave + check, preview and Finalise of one sandbox, and the
//! sandbox a template starts (`tengu sandbox new`). Rust decides every
//! verdict; the page draws it.
//!
//! | Use case | Does | Refused |
//! |---|---|---|
//! | [`Builder::state`] | blueprint (`builder.json`) + palette + its status | — |
//! | [`Builder::save`] | writes `builder.json`, returns the status | view-only (403), another sandbox's blueprint (400) |
//! | [`Builder::preview`] | TOML, diff vs `config.toml`, the real loader's verdict (`Config::load` on a temp copy), secrets checklist, sha256 | — (writes nothing) |
//! | [`Builder::finalise`] | the previewed TOML → `config.toml` (old one → `config.toml.prev`) + `builder.json` | view-only (403) · changed since the preview or runtime running (409) · loader refuses (422) |
//! | [`create`] | `sandboxes/<name>/` from a template | bad name, unknown template, the dir exists |
//!
//! View-only: a sandbox with a `config.toml` but no `builder.json` (hand
//! written), or one `config::builder::editable` refuses (`[generation]`,
//! hardened).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::builder::compile::{compile, SecretUse};
use crate::config::builder::palette::{palette, Palette};
use crate::config::builder::template::template;
use crate::config::builder::{editable, sandbox_name_error, Facts, LoadReport};
use crate::domain::blueprint::{Blueprint, Issue, Status};
use crate::ports::builder::{SandboxDrafts, SecretState};

/// Why a request was refused, by HTTP meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BuilderError {
    BadRequest(String),
    Forbidden(String),
    Conflict(String),
    Invalid(String),
    Internal(String),
}

impl std::fmt::Display for BuilderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuilderError::BadRequest(s)
            | BuilderError::Forbidden(s)
            | BuilderError::Conflict(s)
            | BuilderError::Invalid(s)
            | BuilderError::Internal(s) => f.write_str(s),
        }
    }
}

fn internal(e: anyhow::Error) -> BuilderError {
    BuilderError::Internal(format!("{e:#}"))
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct BuilderState {
    pub sandbox: String,
    pub editable: bool,
    pub why_not: Option<String>,
    pub blueprint: Blueprint,
    pub palette: Palette,
    pub status: Status,
}

/// One key of the checklist, with where it is now.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SecretCheck {
    pub env: String,
    pub backend: String,
    pub state: &'static str,
    pub used_by: Vec<String>,
    pub how: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Preview {
    pub status: Status,
    pub toml: String,
    pub current: Option<String>,
    pub diff: String,
    pub load: LoadReport,
    pub sha256: String,
    pub secrets: Vec<SecretCheck>,
    pub kept: Vec<String>,
    pub next: Vec<String>,
    pub editable: bool,
    pub why_not: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Finalised {
    pub written: String,
    pub sha256: String,
    pub backup: Option<String>,
    pub next: Vec<String>,
    pub secrets: Vec<SecretCheck>,
}

/// The builder of one sandbox.
pub(crate) struct Builder {
    sandbox: String,
    facts: Facts,
    store: Arc<dyn SandboxDrafts>,
}

pub(crate) fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn how(env: &str, backend: &str) -> String {
    match backend {
        "env" => format!("export {env}=…   (or a {env}=… line in .env)"),
        "cloudflare" => {
            format!("npx wrangler secret put {env}   (in cloudflare/seal-worker; the Worker keeps it)")
        }
        _ => format!("tengu secret set {env} <value>   (encrypted ~/.tengu/secrets.vault)"),
    }
}

impl Builder {
    pub(crate) fn new(sandbox: &str, facts: Facts, store: Arc<dyn SandboxDrafts>) -> Self {
        Self {
            sandbox: sandbox.to_string(),
            facts,
            store,
        }
    }

    pub(crate) fn sandbox(&self) -> &str {
        &self.sandbox
    }

    /// Why this sandbox is view-only (`None`: editable).
    pub(crate) fn why_not(&self) -> Result<Option<String>, BuilderError> {
        let has_bp = self
            .store
            .read_blueprint(&self.sandbox)
            .map_err(internal)?
            .is_some();
        let current = self.store.read_config(&self.sandbox).map_err(internal)?;
        Ok(match (has_bp, current) {
            (false, Some(_)) => Some(format!(
                "hand-written sandbox: the builder edits sandboxes made by `tengu sandbox new` \
                 (sandboxes/{}/builder.json). Start a new one to build on the canvas.",
                self.sandbox
            )),
            (_, Some(cur)) => editable(&cur),
            (_, None) => None,
        })
    }

    fn blueprint(&self) -> Result<Blueprint, BuilderError> {
        match self.store.read_blueprint(&self.sandbox).map_err(internal)? {
            Some(text) => serde_json::from_str(&text).map_err(|e| {
                BuilderError::Internal(format!(
                    "sandboxes/{}/builder.json does not parse: {e}",
                    self.sandbox
                ))
            }),
            None => Ok(Blueprint {
                schema_version: crate::domain::blueprint::BLUEPRINT_SCHEMA_VERSION,
                sandbox: self.sandbox.clone(),
                nodes: Vec::new(),
                edges: Vec::new(),
                view: Default::default(),
            }),
        }
    }

    fn current(&self) -> Result<Option<String>, BuilderError> {
        self.store.read_config(&self.sandbox).map_err(internal)
    }

    fn own(&self, bp: &Blueprint) -> Result<(), BuilderError> {
        if bp.sandbox != self.sandbox {
            return Err(BuilderError::BadRequest(format!(
                "this is the builder of '{}', not '{}'",
                self.sandbox, bp.sandbox
            )));
        }
        Ok(())
    }

    fn require_editable(&self) -> Result<(), BuilderError> {
        match self.why_not()? {
            Some(why) => Err(BuilderError::Forbidden(why)),
            None => Ok(()),
        }
    }

    pub(crate) fn state(&self) -> Result<BuilderState, BuilderError> {
        let why_not = self.why_not()?;
        let blueprint = self.blueprint()?;
        let current = self.current()?;
        let status = compile(&blueprint, &self.facts, current.as_deref()).status;
        Ok(BuilderState {
            sandbox: self.sandbox.clone(),
            editable: why_not.is_none(),
            why_not,
            blueprint,
            palette: palette(&self.facts),
            status,
        })
    }

    pub(crate) fn save(&self, bp: &Blueprint) -> Result<Status, BuilderError> {
        self.own(bp)?;
        self.require_editable()?;
        let json = serde_json::to_string_pretty(bp).map_err(|e| internal(e.into()))?;
        self.store
            .write_blueprint(&self.sandbox, &json)
            .map_err(internal)?;
        let current = self.current()?;
        Ok(compile(bp, &self.facts, current.as_deref()).status)
    }

    fn secrets(&self, uses: &[SecretUse]) -> Vec<SecretCheck> {
        uses.iter()
            .map(|u| {
                let state = match u.backend.as_str() {
                    "cloudflare" => SecretState::Remote,
                    b => self.store.secret_state(&u.env, b),
                };
                SecretCheck {
                    env: u.env.clone(),
                    backend: u.backend.clone(),
                    state: state.as_str(),
                    used_by: u.used_by.clone(),
                    how: how(&u.env, &u.backend),
                }
            })
            .collect()
    }

    fn next(&self, bp: &Blueprint, secrets: &[SecretCheck]) -> Vec<String> {
        let s = &self.sandbox;
        let mut out: Vec<String> = secrets
            .iter()
            .filter(|c| c.state == "missing")
            .map(|c| c.how.clone())
            .collect();
        out.push(format!("tengu chat --sandbox {s}"));
        if bp.nodes.iter().any(|n| n.kind == "telegram") {
            out.push(format!("tengu telegram --sandbox {s}"));
        }
        if bp.nodes.iter().any(|n| n.kind == "webhook") {
            out.push(format!(
                "tengu webhooks --sandbox {s}   (a build with --features webhooks)"
            ));
        }
        out.push(format!("tengu studio --sandbox {s} --allow-edit   (back to the canvas)"));
        out
    }

    pub(crate) fn preview(&self, bp: &Blueprint) -> Result<Preview, BuilderError> {
        self.own(bp)?;
        let why_not = self.why_not()?;
        let current = self.current()?;
        let compiled = compile(bp, &self.facts, current.as_deref());
        let load = self.store.check_config(&self.sandbox, &compiled.toml);
        let status = with_load_errors(bp, compiled.status, &load);
        let diff = similar::TextDiff::from_lines(current.as_deref().unwrap_or(""), &compiled.toml)
            .unified_diff()
            .context_radius(3)
            .header("config.toml (on disk)", "config.toml (finalised)")
            .to_string();
        let secrets = self.secrets(&compiled.secrets);
        Ok(Preview {
            next: self.next(bp, &secrets),
            sha256: sha256_hex(&compiled.toml),
            status,
            toml: compiled.toml,
            current,
            diff,
            load,
            secrets,
            kept: compiled.kept,
            editable: why_not.is_none(),
            why_not,
        })
    }

    pub(crate) fn finalise(&self, bp: &Blueprint, sha256: &str) -> Result<Finalised, BuilderError> {
        self.own(bp)?;
        self.require_editable()?;
        if let Some(why) = self.store.busy(&self.sandbox) {
            return Err(BuilderError::Conflict(why));
        }
        let current = self.current()?;
        let compiled = compile(bp, &self.facts, current.as_deref());
        let sha = sha256_hex(&compiled.toml);
        if sha != sha256 {
            return Err(BuilderError::Conflict(
                "the config changed since the preview you saw — Validate again".into(),
            ));
        }
        if !compiled.status.ok {
            return Err(BuilderError::Invalid(format!(
                "{} error(s) on the canvas",
                compiled.status.counts.errors
            )));
        }
        let load = self.store.check_config(&self.sandbox, &compiled.toml);
        if !load.ok {
            return Err(BuilderError::Invalid(format!(
                "the config loader refused it:\n- {}",
                load.errors.join("\n- ")
            )));
        }
        let json = serde_json::to_string_pretty(bp).map_err(|e| internal(e.into()))?;
        self.store
            .write_blueprint(&self.sandbox, &json)
            .map_err(internal)?;
        let backup = self
            .store
            .write_config(&self.sandbox, &compiled.toml)
            .map_err(internal)?;
        tracing::info!(
            sandbox = %self.sandbox,
            sha256 = %sha,
            path = %self.store.config_path(&self.sandbox).display(),
            "builder: config.toml finalised"
        );
        let secrets = self.secrets(&compiled.secrets);
        Ok(Finalised {
            written: self.store.config_path(&self.sandbox).display().to_string(),
            sha256: sha,
            backup: backup.map(|p| p.display().to_string()),
            next: self.next(bp, &secrets),
            secrets,
        })
    }
}

/// The loader's errors join the status: one naming `agents.<name>` lands
/// on that agent's card, the rest on the sandbox.
fn with_load_errors(bp: &Blueprint, status: Status, load: &LoadReport) -> Status {
    if load.errors.is_empty() {
        return status;
    }
    let titles: BTreeMap<String, (String, String)> = status
        .nodes
        .iter()
        .map(|(id, n)| (id.clone(), (n.title.clone(), n.subtitle.clone())))
        .collect();
    let kinds: BTreeMap<String, (String, String, String)> = status
        .edges
        .iter()
        .map(|(id, e)| (id.clone(), (e.edge.clone(), e.label.clone(), e.writes.clone())))
        .collect();
    let mut issues = status.issues;
    for err in &load.errors {
        let card = bp.nodes.iter().find(|n| {
            n.kind == "agent"
                && n.fields
                    .get("name")
                    .and_then(|v| v.as_str())
                    .is_some_and(|name| err.contains(&format!("agents.{name}")))
        });
        let issue = Issue::error(format!("config loader: {err}"));
        issues.push(match card {
            Some(n) => issue.node(&n.id),
            None => match bp.nodes.iter().find(|n| n.kind == "sandbox") {
                Some(n) => issue.node(&n.id),
                None => issue,
            },
        });
    }
    Status::fold(bp, issues, &titles, &kinds)
}

/// `sandboxes/<name>/` from template `tpl` (`tengu sandbox new`): both
/// files, the config checked by the real loader first.
pub(crate) fn create(
    store: &dyn SandboxDrafts,
    facts: &Facts,
    name: &str,
    tpl: &str,
) -> Result<PathBuf, BuilderError> {
    if let Some(why) = sandbox_name_error(name) {
        return Err(BuilderError::BadRequest(why));
    }
    let bp = template(tpl, name).ok_or_else(|| {
        BuilderError::BadRequest(format!(
            "no template '{tpl}' (one of: {})",
            crate::config::builder::template::TEMPLATES
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    let compiled = compile(&bp, facts, None);
    let load = store.check_config(name, &compiled.toml);
    if !load.ok {
        return Err(BuilderError::Invalid(format!(
            "template '{tpl}' does not load:\n- {}",
            load.errors.join("\n- ")
        )));
    }
    let json = serde_json::to_string_pretty(&bp).map_err(|e| internal(e.into()))?;
    store.create(name, &json, &compiled.toml).map_err(|e| {
        BuilderError::Conflict(format!("{e:#}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory store: files by (sandbox, name).
    #[derive(Default)]
    struct Mem {
        files: Mutex<BTreeMap<(String, String), String>>,
        busy: Mutex<Option<String>>,
    }

    impl Mem {
        fn get(&self, s: &str, f: &str) -> Option<String> {
            self.files.lock().unwrap().get(&(s.into(), f.into())).cloned()
        }
        fn put(&self, s: &str, f: &str, v: &str) {
            self.files
                .lock()
                .unwrap()
                .insert((s.into(), f.into()), v.into());
        }
    }

    impl SandboxDrafts for Mem {
        fn config_path(&self, s: &str) -> PathBuf {
            PathBuf::from(format!("sandboxes/{s}/config.toml"))
        }
        fn read_blueprint(&self, s: &str) -> anyhow::Result<Option<String>> {
            Ok(self.get(s, "builder.json"))
        }
        fn write_blueprint(&self, s: &str, j: &str) -> anyhow::Result<()> {
            self.put(s, "builder.json", j);
            Ok(())
        }
        fn read_config(&self, s: &str) -> anyhow::Result<Option<String>> {
            Ok(self.get(s, "config.toml"))
        }
        fn check_config(&self, _s: &str, toml: &str) -> LoadReport {
            match toml::from_str::<crate::config::Config>(toml) {
                Ok(_) => LoadReport {
                    ok: true,
                    ..Default::default()
                },
                Err(e) => LoadReport {
                    ok: false,
                    errors: vec![e.to_string()],
                    warnings: vec![],
                },
            }
        }
        fn write_config(&self, s: &str, toml: &str) -> anyhow::Result<Option<PathBuf>> {
            let old = self.get(s, "config.toml");
            if let Some(o) = &old {
                self.put(s, "config.toml.prev", o);
            }
            self.put(s, "config.toml", toml);
            Ok(old.map(|_| PathBuf::from(format!("sandboxes/{s}/config.toml.prev"))))
        }
        fn create(&self, s: &str, j: &str, toml: &str) -> anyhow::Result<PathBuf> {
            if self.get(s, "config.toml").is_some() {
                anyhow::bail!("sandboxes/{s} exists");
            }
            self.put(s, "builder.json", j);
            self.put(s, "config.toml", toml);
            Ok(PathBuf::from(format!("sandboxes/{s}")))
        }
        fn busy(&self, _s: &str) -> Option<String> {
            self.busy.lock().unwrap().clone()
        }
        fn secret_state(&self, env: &str, _b: &str) -> SecretState {
            if env == "OPENROUTER_API_KEY" {
                SecretState::Present
            } else {
                SecretState::Missing
            }
        }
    }

    fn setup() -> (Arc<Mem>, Builder) {
        let mem = Arc::new(Mem::default());
        let facts = Facts::default();
        create(mem.as_ref(), &facts, "demo", "blank").unwrap();
        let b = Builder::new("demo", facts, mem.clone());
        (mem, b)
    }

    #[test]
    fn new_sandbox_then_preview_then_finalise() {
        let (mem, b) = setup();
        let st = b.state().unwrap();
        assert!(st.editable, "{:?}", st.why_not);
        assert!(st.status.ok, "{:?}", st.status.issues);
        let mut bp = st.blueprint;
        bp.nodes
            .iter_mut()
            .find(|n| n.kind == "agent")
            .unwrap()
            .fields
            .insert("model".into(), serde_json::json!("openai/gpt-5"));
        b.save(&bp).unwrap();
        let p = b.preview(&bp).unwrap();
        assert!(p.load.ok && p.status.ok, "{:?}", p.status.issues);
        assert!(p.diff.contains("+model = \"openai/gpt-5\""), "{}", p.diff);
        let or = p.secrets.iter().find(|s| s.env == "OPENROUTER_API_KEY").unwrap();
        assert_eq!(or.state, "present");
        let f = b.finalise(&bp, &p.sha256).unwrap();
        assert_eq!(f.sha256, p.sha256);
        assert!(f.backup.is_some());
        assert_eq!(mem.get("demo", "config.toml").unwrap(), p.toml);
    }

    #[test]
    fn finalise_refuses_a_stale_preview_and_a_running_sandbox() {
        let (mem, b) = setup();
        let bp = b.state().unwrap().blueprint;
        let err = b.finalise(&bp, "0000").unwrap_err();
        assert!(matches!(err, BuilderError::Conflict(_)), "{err}");
        *mem.busy.lock().unwrap() = Some("runtime running".into());
        let p = b.preview(&bp).unwrap();
        let err = b.finalise(&bp, &p.sha256).unwrap_err();
        assert_eq!(err, BuilderError::Conflict("runtime running".into()));
    }

    #[test]
    fn hand_written_and_frozen_sandboxes_are_view_only() {
        let mem = Arc::new(Mem::default());
        mem.put("hand", "config.toml", "[agents.a]\nengine = \"openrouter\"\nmodel = \"m\"\n");
        let b = Builder::new("hand", Facts::default(), mem.clone());
        let st = b.state().unwrap();
        assert!(!st.editable && st.why_not.unwrap().contains("hand-written"));
        let err = b.save(&st.blueprint).unwrap_err();
        assert!(matches!(err, BuilderError::Forbidden(_)));

        mem.put("w1", "config.toml", "[generation]\nid = \"W1\"\n");
        mem.put("w1", "builder.json", "{}");
        let b = Builder::new("w1", Facts::default(), mem);
        assert!(b.why_not().unwrap().unwrap().contains("frozen"));
    }

    #[test]
    fn create_refuses_bad_names_unknown_templates_and_existing_dirs() {
        let mem = Mem::default();
        let f = Facts::default();
        assert!(matches!(
            create(&mem, &f, "Bad Name", "blank"),
            Err(BuilderError::BadRequest(_))
        ));
        assert!(matches!(
            create(&mem, &f, "ok", "nope"),
            Err(BuilderError::BadRequest(_))
        ));
        create(&mem, &f, "ok", "team").unwrap();
        assert!(matches!(
            create(&mem, &f, "ok", "blank"),
            Err(BuilderError::Conflict(_))
        ));
    }

    #[test]
    fn a_blueprint_for_another_sandbox_is_refused() {
        let (_mem, b) = setup();
        let mut bp = b.state().unwrap().blueprint;
        bp.sandbox = "other".into();
        assert!(matches!(b.save(&bp), Err(BuilderError::BadRequest(_))));
    }
}
