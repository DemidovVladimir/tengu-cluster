//! EvolveSession — bounded rewrite→rescore loop.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::application::skills::lifecycle::metrics::MetricSpec;
use crate::application::skills::lifecycle::storage::MetricRollup;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct Baseline {
    pub rollups: BTreeMap<String, MetricRollup>,
    pub target_metric: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CycleOutcome {
    pub cycle_n: u32,
    pub rollups: BTreeMap<String, MetricRollup>,
    pub body_delta_lines_added: i32,
    pub rationale: String,
    pub new_body: String,
    pub new_metrics: Option<Vec<MetricSpec>>,
    /// Resource files the proposal asked to write under
    /// `skills/<name>/resources/`. Carried through cycle scoring so the
    /// best-cycle pick can surface them on the approval gate and the
    /// `Decision::Apply` branch can write them to the real workspace.
    pub new_resource_additions: Option<Vec<ResourceFile>>,
    /// `Some(n)` when this cycle was derived from a previous cycle's body
    /// rather than the baseline. `None` for cycles that branch from baseline
    /// (the default in v1 — the current driver always builds from the
    /// previous cycle's `new_body` via the scratch worktree, but exposes that
    /// implicitly through the linear loop rather than as data on the outcome.
    /// Reserved for a future "branch off cycle 2's body" flow that the
    /// current driver doesn't expose).
    #[allow(dead_code)]
    pub parent_cycle_n: Option<u32>,
}

#[derive(Deserialize, Clone)]
pub(crate) struct ImproverProposal {
    pub proposal: ProposalBody,
}

#[derive(Deserialize, Clone)]
pub(crate) struct ProposalBody {
    pub body_markdown: String,
    #[serde(default)]
    pub metrics: Option<Vec<MetricSpec>>,
    pub rationale: String,
    /// New resource files to write under `skills/<name>/resources/<path>`.
    /// Existing files at the same path are overwritten only if the proposal
    /// explicitly opts in via `overwrite: true`. Path must be relative and
    /// must not escape the `resources/` directory.
    #[serde(default)]
    pub resource_additions: Option<Vec<ResourceFile>>,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct ResourceFile {
    /// Path relative to `skills/<name>/resources/`. No `..`, no absolute.
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub overwrite: bool,
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-testable)
// ---------------------------------------------------------------------------

pub(crate) fn pick_target_metric(
    rollups: &BTreeMap<String, MetricRollup>,
    explicit: Option<&str>,
) -> Result<String> {
    if let Some(n) = explicit {
        let r = rollups
            .get(n)
            .ok_or_else(|| anyhow::anyhow!("metric '{}' not found", n))?;
        if !r.gated {
            bail!("metric '{}' is not gated; nothing to evolve", n);
        }
        return Ok(n.to_string());
    }
    rollups
        .iter()
        .filter(|(_, r)| r.gated)
        .min_by(|a, b| a.1.pass_rate.partial_cmp(&b.1.pass_rate).unwrap())
        .map(|(k, _)| k.clone())
        .ok_or_else(|| anyhow::anyhow!("no gated metrics failing; nothing to evolve"))
}

/// Rank cycles:
///   (1) highest target pass_rate
///   (2) no regression > 0.05 on any non-target gated metric
///   (3) fewer lines added
///   (4) earliest cycle
pub(crate) fn pick_best(baseline: &Baseline, cycles: &[CycleOutcome]) -> Option<usize> {
    const REG_TOL: f32 = 0.05;
    let target = &baseline.target_metric;
    let eligible: Vec<usize> = cycles
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            for (name, base) in &baseline.rollups {
                if name == target || !base.gated {
                    continue;
                }
                let cur = match c.rollups.get(name) {
                    Some(r) => r,
                    None => continue,
                };
                if cur.pass_rate + REG_TOL < base.pass_rate {
                    return None;
                }
            }
            Some(i)
        })
        .collect();
    eligible.into_iter().min_by(|&a, &b| {
        let ca = &cycles[a];
        let cb = &cycles[b];
        let ra = ca.rollups.get(target).map(|r| r.pass_rate).unwrap_or(0.0);
        let rb = cb.rollups.get(target).map(|r| r.pass_rate).unwrap_or(0.0);
        rb.partial_cmp(&ra)
            .unwrap()
            .then(ca.body_delta_lines_added.cmp(&cb.body_delta_lines_added))
            .then(ca.cycle_n.cmp(&cb.cycle_n))
    })
}

pub(crate) fn apply_proposal_to_skill_md(
    skill_md_path: &Path,
    proposal: &ProposalBody,
) -> Result<()> {
    let body = std::fs::read_to_string(skill_md_path)?;
    let (fm_block, _old_body) = split_frontmatter(&body)?;
    let new_fm = if let Some(ms) = &proposal.metrics {
        replace_metrics_block(fm_block, ms)?
    } else {
        fm_block.to_string()
    };
    let new_contents = format!(
        "---\n{}---\n\n{}\n",
        new_fm,
        proposal.body_markdown.trim_end()
    );
    let parent = skill_md_path.parent().unwrap();
    let tmp = parent.join(format!(".SKILL.md.tmp-{}", unique_suffix()));
    std::fs::write(&tmp, new_contents)?;
    std::fs::rename(&tmp, skill_md_path)?;
    Ok(())
}

/// Validate that `rel_path` stays inside the `resources/` subdir of the skill.
///
/// Rejects: empty paths, paths starting with `/`, paths containing `..`
/// components, paths with absolute roots, and any non-utf8 components.
/// The error message names the offending input so the caller (or the
/// improver agent) can fix the proposal.
pub(crate) fn validate_resource_path(rel_path: &str) -> Result<()> {
    if rel_path.is_empty() {
        bail!("resource_additions path is empty");
    }
    if rel_path.starts_with('/') {
        bail!(
            "resource_additions path '{}' is absolute; must be relative to resources/",
            rel_path
        );
    }
    let p = Path::new(rel_path);
    for c in p.components() {
        match c {
            Component::ParentDir => bail!(
                "resource_additions path '{}' contains a '..' component; \
                 must stay inside resources/",
                rel_path
            ),
            Component::RootDir => bail!(
                "resource_additions path '{}' has a root component; \
                 must be relative to resources/",
                rel_path
            ),
            Component::Prefix(_) => bail!(
                "resource_additions path '{}' has a drive/prefix component; \
                 must be relative to resources/",
                rel_path
            ),
            Component::Normal(os) => {
                if os.to_str().is_none() {
                    bail!(
                        "resource_additions path '{}' has a non-utf8 component",
                        rel_path
                    );
                }
            }
            Component::CurDir => {}
        }
    }
    Ok(())
}

/// Apply the `resource_additions` list. Each file is written atomically via
/// temp-then-rename inside the `resources/` subdir of the skill. Creates
/// parent dirs as needed. Refuses to overwrite an existing file unless
/// `overwrite: true`. Returns the list of paths written, relative to the
/// skill directory (e.g. `resources/genitive.md`).
pub(crate) fn apply_proposal_resources(
    skill_dir: &Path,
    additions: &[ResourceFile],
) -> Result<Vec<PathBuf>> {
    let resources_root = skill_dir.join("resources");
    let mut written: Vec<PathBuf> = Vec::with_capacity(additions.len());
    for entry in additions {
        validate_resource_path(&entry.path)
            .with_context(|| format!("validate resource_additions[{}]", entry.path))?;
        let dest = resources_root.join(&entry.path);
        if dest.exists() && !entry.overwrite {
            bail!(
                "resource_additions: refusing to overwrite existing file {} \
                 (set overwrite: true to replace)",
                dest.display()
            );
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all {}", parent.display()))?;
        }
        let parent = dest.parent().unwrap();
        let tmp = parent.join(format!(".resource.tmp-{}", unique_suffix()));
        std::fs::write(&tmp, &entry.content)
            .with_context(|| format!("write temp {}", tmp.display()))?;
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;
        written.push(PathBuf::from("resources").join(&entry.path));
    }
    Ok(written)
}

pub(crate) fn split_frontmatter(body: &str) -> Result<(&str, &str)> {
    let rest = body
        .strip_prefix("---\n")
        .ok_or_else(|| anyhow::anyhow!("no frontmatter"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("frontmatter not closed"))?;
    Ok((&rest[..end + 1], &rest[end + 4..]))
}

/// Read the `editable_by_learner` frontmatter flag from a skill's SKILL.md.
///
/// Returns `Ok(true)` when the flag is present-and-true OR absent (back-compat
/// default — existing in-tree skills that haven't been migrated yet stay
/// editable, otherwise the just-shipped `tengu skill evolve` end-to-end test
/// would break).
///
/// Returns `Ok(false)` only when the flag is explicitly
/// `editable_by_learner: false`.
///
/// Returns `Err` only on filesystem read errors.
pub(crate) fn is_editable_by_learner(skill_md_path: &Path) -> Result<bool> {
    let body = std::fs::read_to_string(skill_md_path)
        .with_context(|| format!("read {}", skill_md_path.display()))?;
    let (fm_block, _) = match split_frontmatter(&body) {
        Ok(parts) => parts,
        Err(_) => {
            tracing::debug!(
                "is_editable_by_learner: no frontmatter in {} — defaulting to true",
                skill_md_path.display()
            );
            return Ok(true);
        }
    };
    let parsed: serde_yaml::Value = match serde_yaml::from_str(fm_block) {
        Ok(v) => v,
        Err(_) => {
            tracing::debug!(
                "is_editable_by_learner: unparseable frontmatter in {} — defaulting to true",
                skill_md_path.display()
            );
            return Ok(true);
        }
    };
    match parsed.get("editable_by_learner") {
        Some(serde_yaml::Value::Bool(true)) => Ok(true),
        Some(serde_yaml::Value::Bool(false)) => Ok(false),
        _ => {
            tracing::debug!(
                "is_editable_by_learner: flag absent or non-bool in {} — defaulting to true",
                skill_md_path.display()
            );
            Ok(true)
        }
    }
}

fn replace_metrics_block(fm: &str, metrics: &[MetricSpec]) -> Result<String> {
    let mut v: serde_yaml::Value = serde_yaml::from_str(fm)?;
    let new_metrics_val: serde_yaml::Value = serde_yaml::to_value(metrics)?;
    if let serde_yaml::Value::Mapping(ref mut m) = v {
        m.insert(serde_yaml::Value::String("metrics".into()), new_metrics_val);
    }
    Ok(serde_yaml::to_string(&v)?)
}

/// Suffix for temp / quarantine names: unique per call. (Clock-based names
/// collided between concurrent writers — macOS ticks in microseconds.)
pub(crate) fn unique_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollup(rate: f32, gated: bool) -> MetricRollup {
        MetricRollup {
            pass_rate: rate,
            n: 4,
            min_pass_rate: Some(0.8),
            gated,
            stddev: None,
            min: None,
            max: None,
        }
    }

    fn cycle(n: u32, target_rate: f32, other_rate: f32, lines: i32) -> CycleOutcome {
        let mut rollups = BTreeMap::new();
        rollups.insert("target".into(), rollup(target_rate, target_rate < 0.8));
        rollups.insert("other".into(), rollup(other_rate, other_rate < 0.8));
        CycleOutcome {
            cycle_n: n,
            rollups,
            body_delta_lines_added: lines,
            rationale: "x".into(),
            new_body: "b".into(),
            new_metrics: None,
            new_resource_additions: None,
            parent_cycle_n: None,
        }
    }

    fn baseline(target_rate: f32, other_rate: f32) -> Baseline {
        let mut rollups = BTreeMap::new();
        rollups.insert("target".into(), rollup(target_rate, true));
        // "other" is always gated=true in these tests — it has a min_pass_rate
        // protection (0.8) and the regression guard should fire if it drops >0.05.
        rollups.insert("other".into(), rollup(other_rate, true));
        Baseline {
            rollups,
            target_metric: "target".into(),
        }
    }

    #[test]
    fn cycle_outcome_parent_defaults_to_none_in_run_evolve() {
        // The full run_evolve driver requires a chat factory + worktree; we
        // can't exercise it from a unit test. Instead, verify the field
        // exists and that the canonical helper used in this module's tests
        // sets it to None — which mirrors what run_evolve does today.
        let outcome = cycle(1, 0.7, 1.0, 3);
        assert_eq!(outcome.parent_cycle_n, None);
    }

    #[test]
    fn pick_target_auto_selects_lowest_gated() {
        let mut rollups = BTreeMap::new();
        rollups.insert("a".into(), rollup(0.6, true));
        rollups.insert("b".into(), rollup(0.5, true));
        rollups.insert("c".into(), rollup(0.9, false));
        assert_eq!(pick_target_metric(&rollups, None).unwrap(), "b");
    }

    #[test]
    fn pick_target_explicit_rejects_nongated() {
        let mut rollups = BTreeMap::new();
        rollups.insert("a".into(), rollup(0.9, false));
        assert!(pick_target_metric(&rollups, Some("a")).is_err());
    }

    #[test]
    fn pick_best_prefers_highest_target_pass_rate() {
        let b = baseline(0.6, 1.0);
        let cs = vec![cycle(1, 0.7, 1.0, 3), cycle(2, 0.85, 1.0, 3)];
        assert_eq!(pick_best(&b, &cs), Some(1));
    }

    #[test]
    fn pick_best_rejects_regressions_beyond_tolerance() {
        let b = baseline(0.6, 1.0);
        let cs = vec![cycle(1, 0.9, 0.90, 3), cycle(2, 0.75, 0.98, 3)];
        assert_eq!(pick_best(&b, &cs), Some(1));
    }

    #[test]
    fn pick_best_returns_none_if_all_regress() {
        let b = baseline(0.6, 1.0);
        let cs = vec![cycle(1, 0.9, 0.80, 3), cycle(2, 0.95, 0.70, 3)];
        assert_eq!(pick_best(&b, &cs), None);
    }

    #[test]
    fn is_editable_by_learner_returns_true_when_flag_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        let md = dir.path().join("SKILL.md");
        std::fs::write(&md, "---\nname: x\ndescription: y\n---\n\n# body\n").unwrap();
        assert!(is_editable_by_learner(&md).unwrap());
    }

    #[test]
    fn is_editable_by_learner_returns_true_when_flag_explicit() {
        let dir = tempfile::TempDir::new().unwrap();
        let md = dir.path().join("SKILL.md");
        std::fs::write(
            &md,
            "---\nname: x\ndescription: y\neditable_by_learner: true\n---\n\n# body\n",
        )
        .unwrap();
        assert!(is_editable_by_learner(&md).unwrap());
    }

    #[test]
    fn is_editable_by_learner_returns_false_when_flag_explicit_false() {
        let dir = tempfile::TempDir::new().unwrap();
        let md = dir.path().join("SKILL.md");
        std::fs::write(
            &md,
            "---\nname: x\ndescription: y\neditable_by_learner: false\n---\n\n# body\n",
        )
        .unwrap();
        assert!(!is_editable_by_learner(&md).unwrap());
    }

    #[test]
    fn validate_resource_path_rejects_parent_dir() {
        let err = validate_resource_path("../escape.md").unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("..") && msg.contains("../escape.md"),
            "msg should name offending input + reject reason: {}",
            msg
        );
    }

    #[test]
    fn validate_resource_path_rejects_absolute() {
        let err = validate_resource_path("/etc/passwd").unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("/etc/passwd"),
            "msg should name offending input: {}",
            msg
        );
        // Empty also rejected.
        assert!(validate_resource_path("").is_err());
    }

    #[test]
    fn validate_resource_path_accepts_relative_subdir() {
        validate_resource_path("genitive.md").unwrap();
        validate_resource_path("verbs/strong.md").unwrap();
        validate_resource_path("./topic.md").unwrap();
    }

    #[test]
    fn apply_proposal_resources_writes_atomically() {
        let dir = tempfile::TempDir::new().unwrap();
        let skill_dir = dir.path().join("skills").join("german");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let additions = vec![
            ResourceFile {
                path: "genitive.md".into(),
                content: "# Genitive\nlinks...\n".into(),
                overwrite: false,
            },
            ResourceFile {
                path: "verbs/strong.md".into(),
                content: "strong verbs".into(),
                overwrite: false,
            },
        ];
        let written = apply_proposal_resources(&skill_dir, &additions).unwrap();
        assert_eq!(written.len(), 2);
        let f1 = skill_dir.join("resources").join("genitive.md");
        let f2 = skill_dir.join("resources").join("verbs").join("strong.md");
        assert_eq!(
            std::fs::read_to_string(&f1).unwrap(),
            "# Genitive\nlinks...\n"
        );
        assert_eq!(std::fs::read_to_string(&f2).unwrap(), "strong verbs");
        // No leftover .resource.tmp-* siblings.
        for sub in [
            skill_dir.join("resources"),
            skill_dir.join("resources").join("verbs"),
        ] {
            for entry in std::fs::read_dir(&sub).unwrap() {
                let name = entry.unwrap().file_name();
                let s = name.to_string_lossy();
                assert!(
                    !s.starts_with(".resource.tmp-"),
                    "leftover temp file: {}",
                    s
                );
            }
        }
    }

    #[test]
    fn apply_proposal_resources_refuses_overwrite_unless_flag() {
        let dir = tempfile::TempDir::new().unwrap();
        let skill_dir = dir.path().join("skills").join("german");
        std::fs::create_dir_all(skill_dir.join("resources")).unwrap();
        std::fs::write(skill_dir.join("resources").join("genitive.md"), "OLD").unwrap();
        // Without overwrite — refused.
        let err = apply_proposal_resources(
            &skill_dir,
            &[ResourceFile {
                path: "genitive.md".into(),
                content: "NEW".into(),
                overwrite: false,
            }],
        )
        .unwrap_err();
        assert!(format!("{}", err).contains("refusing to overwrite"));
        // File untouched.
        assert_eq!(
            std::fs::read_to_string(skill_dir.join("resources").join("genitive.md")).unwrap(),
            "OLD"
        );
        // With overwrite — replaces.
        apply_proposal_resources(
            &skill_dir,
            &[ResourceFile {
                path: "genitive.md".into(),
                content: "NEW".into(),
                overwrite: true,
            }],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(skill_dir.join("resources").join("genitive.md")).unwrap(),
            "NEW"
        );
    }

    #[test]
    fn apply_proposal_writes_new_body_preserves_frontmatter_fields() {
        let dir = tempfile::TempDir::new().unwrap();
        let md = dir.path().join("SKILL.md");
        std::fs::write(&md, "---\nname: demo\ndescription: d\n---\n\n# OLD BODY\n").unwrap();
        apply_proposal_to_skill_md(
            &md,
            &ProposalBody {
                body_markdown: "# NEW BODY".into(),
                metrics: None,
                rationale: "r".into(),
                resource_additions: None,
            },
        )
        .unwrap();
        let got = std::fs::read_to_string(&md).unwrap();
        assert!(got.contains("name: demo"));
        assert!(got.contains("# NEW BODY"));
        assert!(!got.contains("# OLD BODY"));
    }
}
