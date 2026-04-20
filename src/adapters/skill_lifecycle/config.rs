//! Config for the skill-lifecycle subsystem. Parses the `[skill_lifecycle]`
//! TOML section of `tengu.toml`.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SkillLifecycleConfig {
    pub improver_agent: String,
    pub fixture_runner_agent: String,
    #[serde(default = "default_max_evolve_cycles")]
    pub default_max_evolve_cycles: u32,
    #[serde(default = "default_per_run_dir")]
    // NOTE: Task 7 must read this field to control the per-run directory name.
    pub per_run_dir: String,
    #[serde(default = "default_rolling_window")]
    pub default_rolling_window: u32,
}

fn default_max_evolve_cycles() -> u32 { 3 }
fn default_per_run_dir() -> String { "metrics".to_string() }
fn default_rolling_window() -> u32 { 10 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_with_defaults() {
        let toml = r#"
improver_agent = "skill-improver"
fixture_runner_agent = "fixture-runner"
"#;
        let cfg: SkillLifecycleConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.default_max_evolve_cycles, 3);
        assert_eq!(cfg.default_rolling_window, 10);
        assert_eq!(cfg.per_run_dir, "metrics");
    }

    #[test]
    fn parses_with_overrides() {
        let toml = r#"
improver_agent = "skill-improver"
fixture_runner_agent = "fixture-runner"
default_max_evolve_cycles = 5
default_rolling_window = 20
"#;
        let cfg: SkillLifecycleConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.default_max_evolve_cycles, 5);
        assert_eq!(cfg.default_rolling_window, 20);
    }
}
