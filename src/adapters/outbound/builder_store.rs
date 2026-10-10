//! `FsDrafts` — the Studio builder's sandbox files (`ports::builder`):
//! `<root>/<sandbox>/builder.json` and `config.toml`, `<root>` =
//! `sandboxes/` under the working directory (where `--sandbox <name>` looks).
//!
//! | Op | How |
//! |---|---|
//! | write | temp file in the same dir + rename (a crash leaves the old file) |
//! | `write_config` | the old `config.toml` copied to `config.toml.prev` first |
//! | `check_config` | `config::builder::load_report` on `<tmp>/sandboxes/<sandbox>/config.toml` |
//! | `create` | `create_dir` (an existing dir is refused), then both files |
//! | `busy` | the injected probe (`bootstrap/builder.rs`: a fresh `tengu run` heartbeat) |
//! | `secret_state` | the env var is set and non-empty in this process (the vault and `.env` load there) |

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::builder::{load_report, LoadReport};
use crate::ports::builder::{SandboxDrafts, SecretState};

/// Says why a sandbox's runtime is running, if it is.
pub(crate) type BusyProbe = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub(crate) struct FsDrafts {
    root: PathBuf,
    busy: BusyProbe,
}

pub(crate) const BLUEPRINT_FILE: &str = "builder.json";
const CONFIG_FILE: &str = "config.toml";

impl FsDrafts {
    pub(crate) fn new(root: impl Into<PathBuf>, busy: BusyProbe) -> Self {
        Self {
            root: root.into(),
            busy,
        }
    }

    fn dir(&self, sandbox: &str) -> PathBuf {
        self.root.join(sandbox)
    }
}

fn read_opt(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// `text` → `path` through a temp file in the same directory + rename.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().context("no parent dir")?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("temp file in {}", dir.display()))?;
    tmp.write_all(text.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

impl SandboxDrafts for FsDrafts {
    fn config_path(&self, sandbox: &str) -> PathBuf {
        self.dir(sandbox).join(CONFIG_FILE)
    }

    fn read_blueprint(&self, sandbox: &str) -> Result<Option<String>> {
        read_opt(&self.dir(sandbox).join(BLUEPRINT_FILE))
    }

    fn write_blueprint(&self, sandbox: &str, json: &str) -> Result<()> {
        let dir = self.dir(sandbox);
        if !dir.is_dir() {
            bail!("no sandbox dir {}", dir.display());
        }
        write_atomic(&dir.join(BLUEPRINT_FILE), json)
    }

    fn read_config(&self, sandbox: &str) -> Result<Option<String>> {
        read_opt(&self.config_path(sandbox))
    }

    fn check_config(&self, sandbox: &str, toml: &str) -> LoadReport {
        let run = || -> Result<LoadReport> {
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
        let path = self.config_path(sandbox);
        let backup = if path.is_file() {
            let prev = self.dir(sandbox).join("config.toml.prev");
            std::fs::copy(&path, &prev).with_context(|| format!("back up {}", path.display()))?;
            Some(prev)
        } else {
            None
        };
        write_atomic(&path, toml)?;
        Ok(backup)
    }

    fn create(&self, sandbox: &str, blueprint_json: &str, toml: &str) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        let dir = self.dir(sandbox);
        std::fs::create_dir(&dir).with_context(|| {
            format!(
                "{} exists — pick another name (the builder never overwrites a sandbox)",
                dir.display()
            )
        })?;
        write_atomic(&dir.join(BLUEPRINT_FILE), blueprint_json)?;
        write_atomic(&dir.join(CONFIG_FILE), toml)?;
        Ok(dir)
    }

    fn busy(&self, sandbox: &str) -> Option<String> {
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
