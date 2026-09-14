//! Config for the skill-lifecycle subsystem. Parses the `[skill_lifecycle]`
//! TOML section of `tengu.toml`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SkillLifecycleConfig {
    pub improver_agent: String,
    /// Accepted for backward compatibility; nothing reads it. `tengu eval`
    /// runs fixtures through the eval config's default agent.
    #[serde(default)]
    pub fixture_runner_agent: Option<String>,
    #[serde(default = "default_max_evolve_cycles")]
    pub default_max_evolve_cycles: u32,
    #[serde(default = "default_per_run_dir")]
    // NOTE: Task 7 must read this field to control the per-run directory name.
    pub per_run_dir: String,
    #[serde(default = "default_rolling_window")]
    pub default_rolling_window: u32,
    /// Retain at most this many per-run detail directories under
    /// `skills/<name>/metrics/runs/` and `evals/runs/`. Older ones are pruned
    /// after each successful run. `0` disables pruning (unbounded).
    #[serde(default = "default_max_per_run_reports")]
    pub max_per_run_reports: u32,
    /// Startup sweep: remove scratch worktrees under `.tengu/worktrees/` older
    /// than this many hours (catches leaked worktrees from Ctrl-C / crashes).
    /// `0` disables the sweep.
    #[serde(default = "default_worktree_stale_hours")]
    pub worktree_stale_hours: u32,
}

fn default_max_evolve_cycles() -> u32 {
    3
}
fn default_per_run_dir() -> String {
    "metrics".to_string()
}
fn default_rolling_window() -> u32 {
    10
}
fn default_max_per_run_reports() -> u32 {
    10
}
fn default_worktree_stale_hours() -> u32 {
    24
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_with_defaults() {
        let toml = r#"
improver_agent = "skill-improver"
"#;
        let cfg: SkillLifecycleConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.fixture_runner_agent, None);
        assert_eq!(cfg.default_max_evolve_cycles, 3);
        assert_eq!(cfg.default_rolling_window, 10);
        assert_eq!(cfg.per_run_dir, "metrics");
        assert_eq!(cfg.max_per_run_reports, 10);
        assert_eq!(cfg.worktree_stale_hours, 24);
    }

    #[test]
    fn parses_with_overrides() {
        let toml = r#"
improver_agent = "skill-improver"
fixture_runner_agent = "fixture-runner"
default_max_evolve_cycles = 5
default_rolling_window = 20
max_per_run_reports = 3
worktree_stale_hours = 48
"#;
        let cfg: SkillLifecycleConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.fixture_runner_agent.as_deref(), Some("fixture-runner"));
        assert_eq!(cfg.default_max_evolve_cycles, 5);
        assert_eq!(cfg.default_rolling_window, 20);
        assert_eq!(cfg.max_per_run_reports, 3);
        assert_eq!(cfg.worktree_stale_hours, 48);
    }
}
