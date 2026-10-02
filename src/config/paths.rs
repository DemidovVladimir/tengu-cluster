//! Filesystem locations the config layer resolves: `TENGU_HOME`, the default
//! config file, and `~` expansion for configured paths.

use std::path::{Path, PathBuf};

pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~/") {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(&s[2..]);
        }
    }
    path.to_path_buf()
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
        .unwrap_or_else(|| resolve_tengu_home().join("config.toml"))
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

#[cfg(test)]
mod tests {
    use super::*;

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
