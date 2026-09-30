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
}
