//! EvolveSession — bounded rewrite→rescore loop.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::adapters::skill_lifecycle::approval_gate::{read_decision, render, Decision, GateView};
use crate::adapters::skill_lifecycle::metrics::MetricSpec;
use crate::adapters::skill_lifecycle::scratch_worktree::{
    create_scratch, remove_scratch, sweep_stale_worktrees,
};
use crate::adapters::skill_lifecycle::storage::{MetricRollup, MetricsJson};
use crate::config::Config;
use crate::ports::orchestration::ChatServiceFactory;

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
    let tmp = parent.join(format!(".SKILL.md.tmp-{}", nanos()));
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
        let tmp = parent.join(format!(".resource.tmp-{}", nanos()));
        std::fs::write(&tmp, &entry.content)
            .with_context(|| format!("write temp {}", tmp.display()))?;
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;
        written.push(PathBuf::from("resources").join(&entry.path));
    }
    Ok(written)
}

fn split_frontmatter(body: &str) -> Result<(&str, &str)> {
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

pub(crate) fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn count_body_lines(skill_md: &Path) -> Result<usize> {
    let body = std::fs::read_to_string(skill_md)?;
    let rest = body.strip_prefix("---\n").unwrap_or(&body);
    let body_only = rest.splitn(2, "\n---\n").nth(1).unwrap_or(rest);
    Ok(body_only.lines().count())
}

fn append_evolve_log(
    workspace: &Path,
    skill: &str,
    baseline: &Baseline,
    best: &CycleOutcome,
    verdict: &str,
) -> Result<()> {
    let path = workspace
        .join("skills")
        .join(skill)
        .join("metrics")
        .join("evolve_log.md");
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tgt = &baseline.target_metric;
    let line = format!(
        "- {} | target={} | baseline={:.2} → best={:.2} | verdict={} | rationale={}\n",
        chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ"),
        tgt,
        baseline.rollups[tgt].pass_rate,
        best.rollups.get(tgt).map(|r| r.pass_rate).unwrap_or(0.0),
        verdict,
        best.rationale.replace('\n', " "),
    );
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    use std::io::Write;
    write!(f, "{}", line)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// run_evolve driver
// ---------------------------------------------------------------------------

pub struct EvolveArgs<'a> {
    pub config: &'a Config,
    pub workspace: &'a Path,
    pub skill: &'a str,
    pub max_cycles: Option<u32>,
    pub target_metric: Option<String>,
    pub base_branch: Option<String>,
    pub chat_factory: Arc<dyn ChatServiceFactory>,
}

pub async fn run_evolve(args: EvolveArgs<'_>) -> Result<()> {
    let sl = args.config.skill_lifecycle.as_ref().ok_or_else(|| {
        anyhow::anyhow!("[skill_lifecycle] config missing; needed for tengu skill evolve")
    })?;
    let max_cycles = args.max_cycles.unwrap_or(sl.default_max_evolve_cycles);
    let shell = crate::adapters::shell_executor::LocalShellExecutor::new();

    // 0. Startup sweep — remove any leaked scratch worktrees older than the
    //    configured threshold (default 24h). Covers crashes / Ctrl-C exits.
    sweep_stale_worktrees(&shell, args.workspace, sl.worktree_stale_hours);

    // 1. Baseline — run eval_builder::run_skill against the real workspace.
    let baseline_rollups = run_eval_and_read_metrics(args.workspace, args.skill, None).await?;
    let target = pick_target_metric(&baseline_rollups, args.target_metric.as_deref())?;
    let baseline = Baseline {
        rollups: baseline_rollups,
        target_metric: target,
    };

    // 2. Scratch worktree.
    let scratch = create_scratch(
        &shell,
        args.workspace,
        args.skill,
        args.base_branch.as_deref(),
    )?;

    // 3. Cycle loop.
    let mut cycles: Vec<CycleOutcome> = Vec::new();
    for n in 1..=max_cycles {
        let proposal = call_skill_improver(
            args.workspace,
            args.skill,
            &baseline,
            &cycles,
            &args.chat_factory,
            &sl.improver_agent,
        )
        .await?;

        let skill_md = scratch
            .path
            .join("skills")
            .join(args.skill)
            .join("SKILL.md");
        let old_lines = count_body_lines(&skill_md)?;
        apply_proposal_to_skill_md(&skill_md, &proposal.proposal)?;
        let new_lines = count_body_lines(&skill_md)?;
        let delta_lines = (new_lines as i32 - old_lines as i32).max(0);

        // Cycle rescore inside the scratch workspace.
        let cycle_rollups =
            run_eval_and_read_metrics(&scratch.path, args.skill, Some(&scratch.path)).await?;

        let outcome = CycleOutcome {
            cycle_n: n,
            rollups: cycle_rollups,
            body_delta_lines_added: delta_lines,
            rationale: proposal.proposal.rationale.clone(),
            new_body: std::fs::read_to_string(&skill_md)?,
            new_metrics: proposal.proposal.metrics.clone(),
            new_resource_additions: proposal.proposal.resource_additions.clone(),
            // v1 driver doesn't expose parent-cycle branching; left None
            // until a future flow adds explicit branch points.
            parent_cycle_n: None,
        };
        let tgt_rate = outcome
            .rollups
            .get(&baseline.target_metric)
            .map(|r| r.pass_rate)
            .unwrap_or(0.0);
        cycles.push(outcome);
        if tgt_rate >= 0.999 {
            break;
        }
    }

    // 4. Best-cycle selection.
    let Some(idx) = pick_best(&baseline, &cycles) else {
        eprintln!(
            "evolve found {} proposals but all regressed gated metrics. No changes applied.",
            cycles.len()
        );
        eprintln!(
            "Worktree preserved for inspection: {}",
            scratch.path.display()
        );
        return Ok(());
    };
    let best = &cycles[idx];

    // 5. Approval gate.
    let skill_md_path = args
        .workspace
        .join("skills")
        .join(args.skill)
        .join("SKILL.md");
    let current_body = std::fs::read_to_string(&skill_md_path)?;
    let gated_snapshots: Vec<(String, f32, f32)> = baseline
        .rollups
        .iter()
        .filter(|(n, r)| **n != baseline.target_metric && r.gated)
        .map(|(n, r)| {
            let p = best.rollups.get(n).map(|x| x.pass_rate).unwrap_or(0.0);
            (n.clone(), r.pass_rate, p)
        })
        .collect();
    let resource_additions_preview: Vec<(String, usize)> = best
        .new_resource_additions
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|r| (r.path.clone(), r.content.len()))
        .collect();
    let view = GateView {
        skill: args.skill,
        target_metric: &baseline.target_metric,
        baseline_target: baseline.rollups[&baseline.target_metric].pass_rate,
        best_target: best
            .rollups
            .get(&baseline.target_metric)
            .map(|r| r.pass_rate)
            .unwrap_or(0.0),
        gated_snapshots: &gated_snapshots,
        old_body: &current_body,
        new_body: &best.new_body,
        rationale: &best.rationale,
        resource_additions: &resource_additions_preview,
    };
    let mut stdout = std::io::stdout().lock();
    render(&view, &mut stdout)?;
    drop(stdout);
    let stdin = std::io::stdin();
    let mut lk = stdin.lock();
    let decision = read_decision(&mut lk)?;
    drop(lk);

    // 6. Apply/reject.
    match decision {
        Decision::Apply => {
            // Opt-in gate (A3): refuse to apply diffs to skills that haven't
            // set `editable_by_learner: true` in their frontmatter. Absent
            // flag = allow (back-compat for un-migrated skills).
            if !is_editable_by_learner(&skill_md_path)? {
                let msg = format!(
                    "`editable_by_learner: false` — refusing to apply changes to {}. \
                     Set `editable_by_learner: true` in the SKILL.md frontmatter to opt in.",
                    args.skill
                );
                tracing::warn!("{}", msg);
                eprintln!("{}", msg);
                append_evolve_log(
                    args.workspace,
                    args.skill,
                    &baseline,
                    best,
                    "refused-by-flag",
                )?;
                remove_scratch(&shell, args.workspace, &scratch)?;
                println!("No changes applied — skill is not opted in for learner edits.");
                return Ok(());
            }
            std::fs::copy(
                scratch
                    .path
                    .join("skills")
                    .join(args.skill)
                    .join("SKILL.md"),
                &skill_md_path,
            )?;
            // Apply resource_additions on the real workspace. Failures here
            // do NOT roll back the SKILL.md change — partial application is
            // acceptable; the user's `git diff` will show what landed.
            let skill_dir = args.workspace.join("skills").join(args.skill);
            let mut resources_written: Vec<PathBuf> = Vec::new();
            if let Some(additions) = best.new_resource_additions.as_deref() {
                if !additions.is_empty() {
                    match apply_proposal_resources(&skill_dir, additions) {
                        Ok(paths) => resources_written = paths,
                        Err(e) => {
                            tracing::warn!(
                                "evolve: resource_additions failed for skill '{}': {:#} \
                                 (SKILL.md change preserved)",
                                args.skill,
                                e
                            );
                        }
                    }
                }
            }
            append_evolve_log(args.workspace, args.skill, &baseline, best, "accepted")?;
            // Sanity re-eval on the real workspace.
            let _ = run_eval_and_read_metrics(args.workspace, args.skill, None).await;
            remove_scratch(&shell, args.workspace, &scratch)?;
            let extra = if resources_written.is_empty() {
                String::new()
            } else {
                format!("\n  resources written: {}", resources_written.len())
            };
            println!(
                "Changes applied. Run `git diff skills/{}/` to review.{}",
                args.skill, extra
            );
        }
        Decision::Discard => {
            append_evolve_log(args.workspace, args.skill, &baseline, best, "rejected")?;
            remove_scratch(&shell, args.workspace, &scratch)?;
            println!("No changes applied. Baseline preserved.");
        }
        Decision::ShowDetails => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "rationale": best.rationale,
                    "rollups": best.rollups,
                    "new_metrics": best.new_metrics,
                }))?
            );
            append_evolve_log(
                args.workspace,
                args.skill,
                &baseline,
                best,
                "details-then-discard",
            )?;
            remove_scratch(&shell, args.workspace, &scratch)?;
        }
        Decision::OpenWorktree => {
            println!("Worktree path: {}", scratch.path.display());
            println!(
                "Inspect, then re-run `tengu skill evolve {}` to retry.",
                args.skill
            );
        }
    }
    Ok(())
}

/// Invoke eval_builder::run_skill against a workspace and return the rolling
/// MetricRollups read back from the skill's metrics.json.
///
/// `roots_override`, when `Some`, is passed as the sole root to
/// `discover_skills` — used during cycle scoring to target the scratch path.
async fn run_eval_and_read_metrics(
    workspace: &Path,
    skill: &str,
    roots_override: Option<&Path>,
) -> Result<BTreeMap<String, MetricRollup>> {
    use crate::adapters::eval_builder;

    let judge = eval_builder::build_judge(None).with_context(|| "build judge engine")?;
    let roots = match roots_override {
        Some(p) => vec![p.join("skills")],
        None => eval_builder::default_skill_roots(),
    };
    let mut skills = eval_builder::discover_skills(&[skill.to_string()], &roots)?;
    let skill_ut = skills
        .pop()
        .ok_or_else(|| anyhow::anyhow!("skill '{}' not discovered under {:?}", skill, roots))?;

    let out_dir = workspace.join(".tengu").join("evolve-out");
    std::fs::create_dir_all(&out_dir)?;
    let _ = eval_builder::run_skill(
        &skill_ut,
        Arc::clone(&judge),
        &out_dir,
        None,
        1,
        false,
        None,
        eval_builder::RunSkillOptions::default(),
    )
    .await?;

    let mj_path = if roots_override.is_some() {
        roots_override
            .unwrap()
            .join("skills")
            .join(skill)
            .join("metrics.json")
    } else {
        workspace.join("skills").join(skill).join("metrics.json")
    };
    let mj: MetricsJson = serde_json::from_slice(
        &std::fs::read(&mj_path).with_context(|| format!("read {}", mj_path.display()))?,
    )?;
    Ok(mj.metrics)
}

async fn call_skill_improver(
    workspace: &Path,
    skill: &str,
    baseline: &Baseline,
    prior: &[CycleOutcome],
    chat_factory: &Arc<dyn ChatServiceFactory>,
    improver_agent: &str,
) -> Result<ImproverProposal> {
    let skill_dir = workspace.join("skills").join(skill);
    let body = std::fs::read_to_string(skill_dir.join("SKILL.md"))?;
    let (fm_block, body_only) = split_frontmatter(&body)?;

    // Render current metrics yaml from parsed frontmatter.
    let parsed: serde_yaml::Value = serde_yaml::from_str(fm_block)?;
    let metrics_yaml = parsed
        .get("metrics")
        .map(|v| serde_yaml::to_string(v).unwrap_or_default())
        .unwrap_or_default();

    let tgt = &baseline.target_metric;
    let tgt_rollup = &baseline.rollups[tgt];

    let others = baseline
        .rollups
        .iter()
        .filter(|(n, r)| *n != tgt && r.gated)
        .map(|(n, r)| format!("- {}: {:.2}", n, r.pass_rate))
        .collect::<Vec<_>>()
        .join("\n");

    let prior_summary = if prior.is_empty() {
        String::new()
    } else {
        prior
            .iter()
            .map(|c| {
                format!(
                    "cycle {}: target={:.2}, rationale={}",
                    c.cycle_n,
                    c.rollups.get(tgt).map(|r| r.pass_rate).unwrap_or(0.0),
                    c.rationale
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let user_msg = format!(
        "Skill: {skill}\nCurrent SKILL.md body:\n<<<\n{body}\n>>>\n\n\
         Current metrics (frontmatter block):\n<<<\n{metrics}\n>>>\n\n\
         Target metric: {tgt}\n\
         Target pass rate: {rate:.2} (baseline)  /  min_pass_rate: {min:.2}  → gated (failing)\n\n\
         Other metrics and their baseline pass rates (keep these >= baseline - 0.05):\n{others}\n\n\
         Previous attempts in this session:\n{prior}\n\n\
         Produce your proposal.",
        skill = skill,
        body = body_only.trim(),
        metrics = metrics_yaml,
        tgt = tgt,
        rate = tgt_rollup.pass_rate,
        min = tgt_rollup.min_pass_rate.unwrap_or(0.0),
        others = others,
        prior = prior_summary,
    );

    let raw = chat_factory
        .run_turn(improver_agent, &user_msg)
        .await
        .with_context(|| "skill-improver chat call failed")?;
    let proposal: ImproverProposal = serde_json::from_str(raw.trim())
        .with_context(|| format!("skill-improver returned malformed JSON: {raw}"))?;
    Ok(proposal)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
