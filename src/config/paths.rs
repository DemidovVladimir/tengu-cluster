//! Filesystem locations the config layer resolves: `TENGU_HOME` and where
//! it came from, the [`Layout`] under it, the default config file, the skill
//! tiers and `~` expansion for configured paths. `tengu paths --sandbox <s>`
//! prints them for one sandbox.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~/") {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(&s[2..]);
        }
    }
    path.to_path_buf()
}

/// The inverse of [`expand_tilde`] for display: a path under the home
/// directory as `~/…`, any other path as it is. The Studio graph shows
/// folded scope roots this way — as the TOML wrote them, the same on every
/// machine.
pub fn contract_tilde(path: &Path) -> String {
    if let Some(rest) = dirs_next::home_dir()
        .filter(|h| h.as_os_str().len() > 1)
        .and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf))
    {
        if !rest.as_os_str().is_empty() {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// The config file in effect for this process and its children (`run-agent`,
/// `tengu mcp-bridge`): pinned by `cli::run` to the base config and by
/// `load_sandbox_or` to the absolute sandbox file.
pub(crate) const TENGU_CONFIG_ENV: &str = "TENGU_CONFIG";

/// Config file used when `--config` is absent: `$TENGU_CONFIG` if set,
/// else `<tengu home>/config.toml`. Shared by the parent CLI and the
/// `run-agent` child so both resolve the same file.
pub(crate) fn default_config_path() -> PathBuf {
    std::env::var_os(TENGU_CONFIG_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| Layout::current().config())
}

/// `path` made absolute — canonical when it exists, else joined onto the
/// cwd. For paths handed to a process with another cwd (`TENGU_CONFIG` →
/// `tengu mcp-bridge`, which runs in the agent workspace).
pub(crate) fn absolute_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|d| d.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    })
}

/// The sandbox a config file belongs to: `<name>` for
/// `…/sandboxes/<name>/config.toml` — what `--sandbox <name>` loads, and the
/// same file through `-c` or `TENGU_CONFIG` (the MCP bridge, a `run-agent`
/// child) — else `None`. Read on the canonical path when the file exists, so
/// every process that loads the same file agrees (a symlinked `sandboxes/`
/// entry included). `SandboxSections::sandbox` — the paper ledger's account
/// owner.
pub fn sandbox_of_config_file(path: &Path) -> Option<String> {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if path.file_name()? != "config.toml" {
        return None;
    }
    let dir = path.parent()?;
    let name = dir.file_name()?.to_str()?;
    (dir.parent()?.file_name()? == "sandboxes" && !name.is_empty()).then(|| name.to_string())
}

/// The skill tiers of `workspace`, highest priority first — a skill name
/// in an earlier tier shadows the same name in a later one:
///
/// | Tier | Dir |
/// |---|---|
/// | `managed` | `<TENGU_HOME>/skills` (`tengu skill install --tier managed`) |
/// | `workspace` | `<workspace>/.tengu/skills` |
/// | `project` | `<workspace>/skills` |
///
/// The skill loader (`skills::registry::skill_directories`, which adds the
/// repo's `skills/` where tengu runs), `view_skill`, `manage_skill`,
/// `skill_distill`, `tengu skill` and `tengu eval` all walk this list.
pub fn skill_tiers(workspace: &Path) -> [(&'static str, PathBuf); 3] {
    [
        ("managed", Layout::current().skills()),
        ("workspace", workspace.join(".tengu").join("skills")),
        ("project", workspace.join("skills")),
    ]
}

/// The dir of skill tier `tier` (`managed` | `workspace` | `project`) for
/// `workspace` ([`skill_tiers`]); `None` for any other name.
pub fn skill_tier_root(workspace: &Path, tier: &str) -> Option<PathBuf> {
    skill_tiers(workspace)
        .into_iter()
        .find(|(name, _)| *name == tier)
        .map(|(_, dir)| dir)
}

pub(crate) fn resolve_tengu_home() -> PathBuf {
    if let Ok(home) = std::env::var("TENGU_HOME") {
        if home.starts_with('~') {
            if let Some(user_home) = dirs_next::home_dir() {
                return user_home.join(&home[2..]); // skip "~/"
            }
        }
        return PathBuf::from(home);
    }
    dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".tengu")
}

/// Where `TENGU_HOME` came from (`tengu paths`). `.env` is the worktree
/// trap: a `tengu` started below the main checkout takes its `.env`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeSource {
    /// Exported before `tengu` started.
    Env,
    /// Set by this `.env` file ([`load_dotenv`]).
    DotEnv(PathBuf),
    /// Unset: `~/.tengu`.
    Default,
}

static HOME_SOURCE: OnceLock<HomeSource> = OnceLock::new();

/// Load the nearest `.env` up from the cwd — never overriding an exported
/// variable — and note where `TENGU_HOME` came from ([`home_source`]).
pub fn load_dotenv() {
    let exported = std::env::var_os("TENGU_HOME").is_some();
    let file = dotenvy::dotenv().ok();
    let source = match file {
        _ if exported => HomeSource::Env,
        Some(f) if std::env::var_os("TENGU_HOME").is_some() => HomeSource::DotEnv(f),
        _ => HomeSource::Default,
    };
    let _ = HOME_SOURCE.set(source);
}

/// What [`load_dotenv`] found; before it ran, the env alone decides.
pub fn home_source() -> HomeSource {
    HOME_SOURCE.get().cloned().unwrap_or_else(|| {
        if std::env::var_os("TENGU_HOME").is_some() {
            HomeSource::Env
        } else {
            HomeSource::Default
        }
    })
}

/// The `TENGU_HOME` tree — the one place its names are joined:
///
/// | Path | Holds |
/// |---|---|
/// | `config.toml` | the base config (no `--sandbox`, `-c` or `TENGU_CONFIG`) |
/// | `state/` | `runtime.db` + heartbeat of a sandbox with no state dir, `solana-writes.db` |
/// | `state/<name>/` | a sandbox's stores: `[xmarket] state`, `[sources] state` (`soe`: the SOE profile) |
/// | `state/evidence/<vault>/` | read-only evidence vaults (`tengu evidence snapshot`) |
/// | `logs/` | `tengu.log`, `egress.jsonl`, `decisions.jsonl`, `risk.jsonl`, `trace/<sandbox>/`, `maps/` |
/// | `skills/` | the managed skill tier |
/// | `memory/` | the vectors of an agent with no workspace and no `[memory] store_path` |
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    home: PathBuf,
}

impl Layout {
    /// The tree at [`resolve_tengu_home`].
    pub fn current() -> Self {
        Self::at(resolve_tengu_home())
    }

    pub fn at(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn config(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    pub fn state(&self) -> PathBuf {
        self.home.join("state")
    }

    /// `state/<name>`.
    pub fn state_dir(&self, name: &str) -> PathBuf {
        self.state().join(name)
    }

    /// `state/evidence/<vault>`.
    pub fn vault(&self, vault: &str) -> PathBuf {
        self.state_dir("evidence").join(vault)
    }

    pub fn logs(&self) -> PathBuf {
        self.home.join("logs")
    }

    pub fn tengu_log(&self) -> PathBuf {
        self.logs().join("tengu.log")
    }

    /// The egress audit unless `[egress] audit_log` names another file.
    pub fn egress_log(&self) -> PathBuf {
        self.logs().join("egress.jsonl")
    }

    pub fn decisions_log(&self) -> PathBuf {
        self.logs().join("decisions.jsonl")
    }

    pub fn risk_log(&self) -> PathBuf {
        self.logs().join("risk.jsonl")
    }

    /// `logs/trace` — one dir per sandbox.
    pub fn trace(&self) -> PathBuf {
        self.logs().join("trace")
    }

    /// `logs/maps` — the execution maps `tengu decide --map` ran.
    pub fn maps(&self) -> PathBuf {
        self.logs().join("maps")
    }

    pub fn skills(&self) -> PathBuf {
        self.home.join("skills")
    }

    pub fn memory(&self) -> PathBuf {
        self.home.join("memory")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `contract_tilde` undoes `expand_tilde`; other paths stay as they are.
    #[test]
    fn contract_tilde_inverts_expand_tilde() {
        let ws = Path::new("~/tengu-lab/control-loop-lab");
        assert_eq!(
            contract_tilde(&expand_tilde(ws)),
            "~/tengu-lab/control-loop-lab"
        );
        assert_eq!(contract_tilde(Path::new("/srv/ws")), "/srv/ws");
        assert_eq!(contract_tilde(Path::new("./ws")), "./ws");
    }

    #[test]
    fn absolute_path_is_canonical_or_cwd_joined() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, "").unwrap();
        assert_eq!(absolute_path(&file), std::fs::canonicalize(&file).unwrap());
        let missing = absolute_path(Path::new("sandboxes/none/config.toml"));
        assert!(missing.is_absolute(), "{}", missing.display());
        assert!(missing.ends_with("sandboxes/none/config.toml"));
    }

    /// The managed skill tier sits under `TENGU_HOME`, never a hard-coded
    /// `~/.tengu`: a lab home reads and writes its own managed skills.
    #[test]
    fn skill_tiers_are_rooted_at_tengu_home() {
        let ws = Path::new("/ws");
        let tiers = skill_tiers(ws);
        let names: Vec<&str> = tiers.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["managed", "workspace", "project"]);
        assert_eq!(tiers[0].1, resolve_tengu_home().join("skills"));
        assert_eq!(tiers[1].1, ws.join(".tengu/skills"));
        assert_eq!(tiers[2].1, ws.join("skills"));
        assert_eq!(skill_tier_root(ws, "project"), Some(ws.join("skills")));
        assert_eq!(skill_tier_root(ws, "global"), None);
    }

    /// Every name under the home is joined here once.
    #[test]
    fn layout_names_the_home_tree() {
        let l = Layout::at("/h");
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(l.home(), Path::new("/h"));
        assert_eq!(l.config(), p("/h/config.toml"));
        assert_eq!(l.state_dir("xm"), p("/h/state/xm"));
        assert_eq!(l.vault("w1"), p("/h/state/evidence/w1"));
        assert_eq!(l.tengu_log(), p("/h/logs/tengu.log"));
        assert_eq!(l.egress_log(), p("/h/logs/egress.jsonl"));
        assert_eq!(l.decisions_log(), p("/h/logs/decisions.jsonl"));
        assert_eq!(l.risk_log(), p("/h/logs/risk.jsonl"));
        assert_eq!(l.trace(), p("/h/logs/trace"));
        assert_eq!(l.maps(), p("/h/logs/maps"));
        assert_eq!(l.skills(), p("/h/skills"));
        assert_eq!(l.memory(), p("/h/memory"));
    }

    /// `--sandbox <name>` and the same file by absolute path (the bridge's
    /// `TENGU_CONFIG`) name one sandbox; any other file names none.
    #[test]
    fn a_config_file_names_its_sandbox() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("sandboxes/xmarket-weekend");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(&file, "").unwrap();
        let named = Some("xmarket-weekend".to_string());
        assert_eq!(sandbox_of_config_file(&file), named);
        assert_eq!(sandbox_of_config_file(&absolute_path(&file)), named);
        // Relative, as `load_sandbox_or` passes it (lexical when missing).
        assert_eq!(
            sandbox_of_config_file(Path::new("sandboxes/nowhere-xyz/config.toml")),
            Some("nowhere-xyz".to_string())
        );
        for other in [
            tmp.path().join("config.toml"),
            dir.join("other.toml"),
            tmp.path().join("fixtures/xmarket/config.toml"),
        ] {
            assert_eq!(sandbox_of_config_file(&other), None, "{}", other.display());
        }
        #[cfg(unix)]
        {
            // A symlinked sandbox dir: named by where it resolves.
            let real = tmp.path().join("elsewhere");
            std::fs::create_dir_all(&real).unwrap();
            std::fs::write(real.join("config.toml"), "").unwrap();
            std::os::unix::fs::symlink(&real, tmp.path().join("sandboxes/linked")).unwrap();
            let linked = tmp.path().join("sandboxes/linked/config.toml");
            assert_eq!(sandbox_of_config_file(&linked), None);
        }
    }
}
