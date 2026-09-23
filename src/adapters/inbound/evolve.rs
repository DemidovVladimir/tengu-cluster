//! `tengu skill evolve` — the evolve loop driver: baseline eval, improver
//! call, candidate evals in a scratch worktree, approval gate, apply. The
//! pure proposal/selection logic lives in
//! `application::skills::lifecycle::evolve`.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::application::skills::lifecycle::approval_gate::{
    read_decision, render, Decision, GateView,
};
use crate::application::skills::lifecycle::evolve::{
    apply_proposal_resources, apply_proposal_to_skill_md, is_editable_by_learner, pick_best,
    pick_target_metric, split_frontmatter, Baseline, CycleOutcome, ImproverProposal,
};
use crate::application::skills::lifecycle::scratch_worktree::{
    create_scratch, remove_scratch, sweep_stale_worktrees,
};

use crate::application::skills::lifecycle::storage::{MetricRollup, MetricsJson};
use crate::config::Config;
use crate::ports::orchestration::ChatServiceFactory;

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
    let shell = crate::adapters::outbound::shell::LocalShellExecutor::new();

    // 0. Startup sweep — remove any leaked scratch worktrees older than the
    //    configured threshold (default 24h). Covers crashes / Ctrl-C exits.
    sweep_stale_worktrees(&shell, args.workspace, sl.worktree_stale_hours);

    // 1. Baseline — run eval::run_skill against the real workspace.
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

/// Invoke eval::run_skill against a workspace and return the rolling
/// MetricRollups read back from the skill's metrics.json.
///
/// `roots_override`, when `Some`, is passed as the sole root to
/// `discover_skills` — used during cycle scoring to target the scratch path.
async fn run_eval_and_read_metrics(
    workspace: &Path,
    skill: &str,
    roots_override: Option<&Path>,
) -> Result<BTreeMap<String, MetricRollup>> {
    use crate::adapters::inbound::eval;

    let judge = eval::build_judge(None).with_context(|| "build judge engine")?;
    let roots = match roots_override {
        Some(p) => vec![p.join("skills")],
        None => eval::default_skill_roots(),
    };
    let mut skills = eval::discover_skills(&[skill.to_string()], &roots)?;
    let skill_ut = skills
        .pop()
        .ok_or_else(|| anyhow::anyhow!("skill '{}' not discovered under {:?}", skill, roots))?;

    let out_dir = workspace.join(".tengu").join("evolve-out");
    std::fs::create_dir_all(&out_dir)?;
    let _ = eval::run_skill(
        &skill_ut,
        Arc::clone(&judge),
        &out_dir,
        None,
        1,
        false,
        None,
        eval::RunSkillOptions::default(),
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
