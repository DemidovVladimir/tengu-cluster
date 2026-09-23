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

/// Config file used when `--config` is absent: `$TENGU_CONFIG` if set,
/// else `<tengu home>/config.toml`. Shared by the parent CLI and the
/// `run-agent` child so both resolve the same file.
pub(crate) fn default_config_path() -> PathBuf {
    std::env::var_os("TENGU_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| resolve_tengu_home().join("config.toml"))
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
