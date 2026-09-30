//! `[runtime]` — knobs of the `tengu run` process (`bootstrap/runtime.rs`;
//! operator doc `docs/runtime-2026-09-30.md`). `deny_unknown_fields`; every
//! key has a default, so a sandbox without `[runtime]` runs as-is.
//!
//! | Key | Default | Effect |
//! |---|---|---|
//! | `shutdown_grace_secs` | 20 | after SIGINT / SIGTERM, running loop events and tasks get this long (≤ 3600), then are aborted |
//! | `max_decisions_in_flight` | 4 | loop events running at once across all loops (each loop runs one event at a time) |
//! | `heartbeat_secs` | 5 | period of the heartbeat file `run-<sandbox>.json` and the `loop/1` / `feed/1` rows (1–3600) |
//! | `heartbeat_stale_secs` | 30 | `tengu doctor --live` fails when the heartbeat is older (> `heartbeat_secs`) |

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Upper bound of `shutdown_grace_secs` / `heartbeat_secs` (a typo must not
/// hang a stop or silence the heartbeat).
const MAX_SECS: u64 = 3600;

/// `[runtime]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    /// Seconds running loop events and runtime tasks get after SIGINT /
    /// SIGTERM before they are aborted. Queued events are dropped at once.
    #[serde(default = "default_shutdown_grace_secs")]
    pub shutdown_grace_secs: u64,
    /// Loop events running at once across every loop (a loop never runs two
    /// at once). Also bounds `tengu webhooks` loop endpoints. Default 4.
    #[serde(default = "default_max_decisions_in_flight")]
    pub max_decisions_in_flight: usize,
    /// Period of the heartbeat file and the health rows. Default 5.
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
    /// `tengu doctor --live` fails when the heartbeat is older than this.
    /// Must exceed `heartbeat_secs`. Default 30.
    #[serde(default = "default_heartbeat_stale_secs")]
    pub heartbeat_stale_secs: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            shutdown_grace_secs: default_shutdown_grace_secs(),
            max_decisions_in_flight: default_max_decisions_in_flight(),
            heartbeat_secs: default_heartbeat_secs(),
            heartbeat_stale_secs: default_heartbeat_stale_secs(),
        }
    }
}

fn default_shutdown_grace_secs() -> u64 {
    20
}

fn default_max_decisions_in_flight() -> usize {
    4
}

fn default_heartbeat_secs() -> u64 {
    5
}

fn default_heartbeat_stale_secs() -> u64 {
    30
}

impl RuntimeConfig {
    pub fn validation_errors(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.max_decisions_in_flight == 0 {
            out.push("runtime.max_decisions_in_flight must be at least 1".to_string());
        }
        if self.shutdown_grace_secs > MAX_SECS {
            out.push(format!(
                "runtime.shutdown_grace_secs {} is above the {MAX_SECS} s limit",
                self.shutdown_grace_secs
            ));
        }
        if !(1..=MAX_SECS).contains(&self.heartbeat_secs) {
            out.push(format!(
                "runtime.heartbeat_secs {} must be 1–{MAX_SECS}",
                self.heartbeat_secs
            ));
        }
        if self.heartbeat_stale_secs <= self.heartbeat_secs {
            out.push(format!(
                "runtime.heartbeat_stale_secs {} must exceed heartbeat_secs {}",
                self.heartbeat_stale_secs, self.heartbeat_secs
            ));
        }
        out
    }
}

/// Directory of `runtime.db` and the heartbeat: the `[xmarket]` state dir
/// when the sandbox has one (`SandboxSections::xm_state_dir`), else
/// `<tengu_home>/state`.
pub fn state_dir(xm_state_dir: Option<&Path>, tengu_home: &Path) -> PathBuf {
    xm_state_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| tengu_home.join("state"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_without_the_section() {
        let r: RuntimeConfig = toml::from_str("").unwrap();
        assert_eq!(r, RuntimeConfig::default());
        assert_eq!((r.shutdown_grace_secs, r.max_decisions_in_flight), (20, 4));
        assert_eq!((r.heartbeat_secs, r.heartbeat_stale_secs), (5, 30));
        assert!(r.validation_errors().is_empty());
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        assert!(toml::from_str::<RuntimeConfig>("shutdown_grace = 5").is_err());
        let r: RuntimeConfig =
            toml::from_str("max_decisions_in_flight = 0\nshutdown_grace_secs = 3601").unwrap();
        let errors = r.validation_errors().join("\n");
        assert!(errors.contains("max_decisions_in_flight"), "{errors}");
        assert!(errors.contains("shutdown_grace_secs"), "{errors}");
        let r: RuntimeConfig =
            toml::from_str("heartbeat_secs = 0\nheartbeat_stale_secs = 0").unwrap();
        let errors = r.validation_errors().join("\n");
        assert!(errors.contains("heartbeat_secs 0 must be"), "{errors}");
        let r: RuntimeConfig =
            toml::from_str("heartbeat_secs = 30\nheartbeat_stale_secs = 30").unwrap();
        let errors = r.validation_errors().join("\n");
        assert!(errors.contains("must exceed heartbeat_secs 30"), "{errors}");
    }

    #[test]
    fn state_dir_prefers_the_xmarket_dir() {
        let home = Path::new("/h");
        assert_eq!(state_dir(None, home), PathBuf::from("/h/state"));
        let xm = Path::new("/h/state/xmarket-weekend");
        assert_eq!(state_dir(Some(xm), home), xm.to_path_buf());
    }
}
