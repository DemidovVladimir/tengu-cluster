//! `FsDrafts` — the Studio builder's sandbox files (`ports::builder`):
//! `<root>/<sandbox>/builder.json` and `config.toml`, `<root>` =
//! `sandboxes/` under the working directory (where `--sandbox <name>` looks).
//!
//! | Op | How |
//! |---|---|
//! | every op | the name passes `config::builder::sandbox_name_error` (no `/`, `.`, absolute path) and an existing dir / file resolves inside the root / the sandbox dir (canonicalized: a symlink out is refused) — before any read or write, whatever the caller checked |
//! | write | temp file in the same dir + rename (a crash leaves the old file; a symlink at the path is replaced, never followed) |
//! | `write_config` | the old `config.toml` written to `config.toml.prev` first (same temp + rename) |
//! | `check_config` | `config::builder::load_report` on `<tmp>/sandboxes/<sandbox>/config.toml` |
//! | `create` | `create_dir` (an existing dir is refused), then both files |
//! | `busy` | the injected probe (`bootstrap/builder.rs`: a fresh `tengu run` heartbeat) |
//! | `secret_state` | the env var is set and non-empty in this process (the vault and `.env` load there) |
//!
//! Check, then use: a process racing a symlink swap between the two is not
//! covered (the operator's own machine).

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::config::builder::{load_report, sandbox_name_error, LoadReport};
use crate::ports::builder::{SandboxDrafts, SecretState};

/// Says why a sandbox's runtime is running, if it is.
pub(crate) type BusyProbe = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub(crate) struct FsDrafts {
    root: PathBuf,
    busy: BusyProbe,
}

pub(crate) const BLUEPRINT_FILE: &str = "builder.json";
const CONFIG_FILE: &str = "config.toml";
const PREV_FILE: &str = "config.toml.prev";

/// The name rule, as an error.
fn checked_name(sandbox: &str) -> Result<&str> {
    match sandbox_name_error(sandbox) {
        None => Ok(sandbox),
        Some(why) => Err(anyhow!("refused sandbox name {why}")),
    }
}

fn exists_or_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// `path` (it exists) resolves strictly inside `within` (canonical).
fn resolves_inside(path: &Path, within: &Path) -> Result<()> {
    let real = std::fs::canonicalize(path)
        .with_context(|| format!("{} does not resolve: refused", path.display()))?;
    if real == within || !real.starts_with(within) {
        bail!(
            "{} resolves to {}, outside {}: refused",
            path.display(),
            real.display(),
            within.display()
        );
    }
    Ok(())
}

impl FsDrafts {
    pub(crate) fn new(root: impl Into<PathBuf>, busy: BusyProbe) -> Self {
        Self {
            root: root.into(),
            busy,
        }
    }

    /// `<root>/<sandbox>`: the name passes the rule and the dir, when it
    /// exists, resolves inside the root.
    fn dir(&self, sandbox: &str) -> Result<PathBuf> {
        let dir = self.root.join(checked_name(sandbox)?);
        if exists_or_link(&dir) {
            let root = std::fs::canonicalize(&self.root)
                .with_context(|| format!("resolve {}", self.root.display()))?;
            resolves_inside(&dir, &root)?;
        }
        Ok(dir)
    }

    /// `<root>/<sandbox>/<file>`: [`Self::dir`], and the file, when it
    /// exists, resolves inside that dir.
    fn file(&self, sandbox: &str, file: &str) -> Result<PathBuf> {
        let dir = self.dir(sandbox)?;
        let path = dir.join(file);
        if exists_or_link(&path) {
            let real_dir = std::fs::canonicalize(&dir)
                .with_context(|| format!("resolve {}", dir.display()))?;
            resolves_inside(&path, &real_dir)?;
        }
        Ok(path)
    }
}

fn read_opt(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// `bytes` → `path` through a temp file in the same directory + rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("no parent dir")?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("temp file in {}", dir.display()))?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

impl SandboxDrafts for FsDrafts {
    fn config_path(&self, sandbox: &str) -> Result<PathBuf> {
        self.file(sandbox, CONFIG_FILE)
    }

    fn read_blueprint(&self, sandbox: &str) -> Result<Option<String>> {
        read_opt(&self.file(sandbox, BLUEPRINT_FILE)?)
    }

    fn write_blueprint(&self, sandbox: &str, json: &str) -> Result<()> {
        let path = self.file(sandbox, BLUEPRINT_FILE)?;
        let dir = path.parent().context("no parent dir")?;
        if !dir.is_dir() {
            bail!("no sandbox dir {}", dir.display());
        }
        write_atomic(&path, json.as_bytes())
    }

    fn read_config(&self, sandbox: &str) -> Result<Option<String>> {
        read_opt(&self.file(sandbox, CONFIG_FILE)?)
    }

    fn check_config(&self, sandbox: &str, toml: &str) -> LoadReport {
        let run = || -> Result<LoadReport> {
            let sandbox = checked_name(sandbox)?;
            let tmp = tempfile::tempdir()?;
            let path = tmp.path().join("sandboxes").join(sandbox).join(CONFIG_FILE);
            std::fs::create_dir_all(path.parent().context("no parent")?)?;
            std::fs::write(&path, toml)?;
            Ok(load_report(&path))
        };
        run().unwrap_or_else(|e| LoadReport {
            ok: false,
            errors: vec![format!("could not check: {e:#}")],
            warnings: Vec::new(),
        })
    }

    fn write_config(&self, sandbox: &str, toml: &str) -> Result<Option<PathBuf>> {
        let path = self.file(sandbox, CONFIG_FILE)?;
        let prev = self.file(sandbox, PREV_FILE)?;
        let backup = if path.is_file() {
            let old = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            write_atomic(&prev, &old).with_context(|| format!("back up {}", path.display()))?;
            Some(prev)
        } else {
            None
        };
        write_atomic(&path, toml.as_bytes())?;
        Ok(backup)
    }

    fn create(&self, sandbox: &str, blueprint_json: &str, toml: &str) -> Result<PathBuf> {
        let name = checked_name(sandbox)?;
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        let dir = self.root.join(name);
        std::fs::create_dir(&dir).with_context(|| {
            format!(
                "{} exists — pick another name (the builder never overwrites a sandbox)",
                dir.display()
            )
        })?;
        let dir = self.dir(sandbox)?;
        write_atomic(
            &self.file(sandbox, BLUEPRINT_FILE)?,
            blueprint_json.as_bytes(),
        )?;
        write_atomic(&self.file(sandbox, CONFIG_FILE)?, toml.as_bytes())?;
        Ok(dir)
    }

    fn busy(&self, sandbox: &str) -> Option<String> {
        if let Err(e) = self.dir(sandbox) {
            return Some(format!("{e:#}"));
        }
        (self.busy)(sandbox)
    }

    fn secret_state(&self, env: &str, _backend: &str) -> SecretState {
        match std::env::var(env) {
            Ok(v) if !v.trim().is_empty() => SecretState::Present,
            _ => SecretState::Missing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(root: &Path) -> FsDrafts {
        FsDrafts::new(root, Box::new(|_| None))
    }

    #[test]
    fn create_writes_both_files_and_refuses_an_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(&tmp.path().join("sandboxes"));
        let dir = s.create("demo", "{}", "x = 1\n").unwrap();
        assert!(dir.join("builder.json").is_file() && dir.join("config.toml").is_file());
        let err = s.create("demo", "{}", "").unwrap_err();
        assert!(format!("{err:#}").contains("exists"), "{err:#}");
        assert_eq!(s.read_blueprint("demo").unwrap().as_deref(), Some("{}"));
        assert_eq!(s.read_blueprint("nope").unwrap(), None);
    }

    #[test]
    fn write_config_keeps_the_previous_file() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(&tmp.path().join("sandboxes"));
        s.create("demo", "{}", "old = 1\n").unwrap();
        let prev = s.write_config("demo", "new = 2\n").unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(prev).unwrap(), "old = 1\n");
        assert_eq!(s.read_config("demo").unwrap().unwrap(), "new = 2\n");
    }

    const TRAVERSAL: &[&str] = &["../x", "a/../../b", "a/b", "/tmp/x", "..", ".", ""];

    #[test]
    fn traversal_names_are_refused_before_any_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sandboxes");
        std::fs::create_dir_all(root.join("a")).unwrap();
        let s = store(&root);
        let abs = tmp.path().join("abs");
        let abs = abs.to_str().unwrap();
        for name in TRAVERSAL.iter().copied().chain([abs]) {
            let refused = |r: Result<()>| {
                let e = r.unwrap_err();
                assert!(
                    format!("{e:#}").contains("refused sandbox name"),
                    "{name}: {e:#}"
                );
            };
            refused(s.config_path(name).map(drop));
            refused(s.read_blueprint(name).map(drop));
            refused(s.read_config(name).map(drop));
            refused(s.write_blueprint(name, "{}"));
            refused(s.write_config(name, "x = 1\n").map(drop));
            refused(s.create(name, "{}", "x = 1\n").map(drop));
            let r = s.check_config(name, "");
            assert!(
                !r.ok && r.errors[0].contains("refused sandbox name"),
                "{r:?}"
            );
            assert!(s.busy(name).unwrap().contains("refused sandbox name"));
        }
        // Nothing was written anywhere under the temp dir but `sandboxes/a`.
        let mut left: Vec<String> = walk(tmp.path());
        left.sort();
        assert_eq!(left, vec!["sandboxes", "sandboxes/a"]);
    }

    fn walk(base: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![base.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                out.push(p.strip_prefix(base).unwrap().display().to_string());
                if p.is_dir() && !p.is_symlink() {
                    stack.push(p);
                }
            }
        }
        out
    }

    #[cfg(unix)]
    #[test]
    fn a_sandbox_dir_linked_outside_the_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("config.toml"), "secret = 1\n").unwrap();
        std::fs::write(outside.join("builder.json"), "{}").unwrap();
        let root = tmp.path().join("sandboxes");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("evil")).unwrap();
        let s = store(&root);
        let refused = |r: Result<()>| {
            let e = r.unwrap_err();
            assert!(format!("{e:#}").contains("outside"), "{e:#}");
        };
        refused(s.read_config("evil").map(drop));
        refused(s.read_blueprint("evil").map(drop));
        refused(s.write_blueprint("evil", "{\"x\":1}"));
        refused(s.write_config("evil", "new = 2\n").map(drop));
        let e = s.create("evil", "{}", "").unwrap_err();
        assert!(format!("{e:#}").contains("exists"), "{e:#}");
        assert!(s.busy("evil").unwrap().contains("outside"));
        assert_eq!(
            std::fs::read_to_string(outside.join("config.toml")).unwrap(),
            "secret = 1\n"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("builder.json")).unwrap(),
            "{}"
        );
        assert!(!outside.join("config.toml.prev").exists());
        // A link that stays inside the root is fine.
        s.create("real", "{}", "a = 1\n").unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
        assert_eq!(s.read_config("alias").unwrap().unwrap(), "a = 1\n");
    }

    #[cfg(unix)]
    #[test]
    fn files_linked_outside_the_sandbox_dir_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("victim.toml");
        std::fs::write(&outside, "victim = 1\n").unwrap();
        let root = tmp.path().join("sandboxes");
        let s = store(&root);
        s.create("demo", "{}", "old = 1\n").unwrap();
        let dir = root.join("demo");
        // `config.toml.prev` → outside: the backup would write through it.
        std::os::unix::fs::symlink(&outside, dir.join("config.toml.prev")).unwrap();
        let e = s.write_config("demo", "new = 2\n").unwrap_err();
        assert!(format!("{e:#}").contains("outside"), "{e:#}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "victim = 1\n");
        assert_eq!(s.read_config("demo").unwrap().unwrap(), "old = 1\n");
        // `config.toml` → outside: neither read nor written.
        std::fs::remove_file(dir.join("config.toml.prev")).unwrap();
        std::fs::remove_file(dir.join("config.toml")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("config.toml")).unwrap();
        assert!(s.read_config("demo").is_err());
        assert!(s.write_config("demo", "new = 2\n").is_err());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "victim = 1\n");
    }

    #[test]
    fn check_config_runs_the_real_loader() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let ok = s.check_config(
            "demo",
            "[egress]\nnetwork = \"open\"\n[agents.a]\nengine = \"openrouter\"\nmodel = \"m\"\n",
        );
        assert!(ok.ok, "{:?}", ok.errors);
        let bad = s.check_config("demo", "[agents.a]\nengine = \"nope\"\nmodel = \"m\"\n");
        assert!(!bad.ok);
        assert!(
            bad.errors.iter().any(|e| e.contains("agents.a.engine")),
            "{:?}",
            bad.errors
        );
        let typo = s.check_config("demo", "[rsik]\n");
        assert!(
            !typo.ok && typo.errors[0].contains("rsik"),
            "{:?}",
            typo.errors
        );
    }
}
