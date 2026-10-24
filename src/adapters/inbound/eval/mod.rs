//! Skill eval runner — `tengu eval <skill>`.
//!
//! Spec: `docs/skill-lifecycle-validation.md (the 2026-04-19 eval-runner design spec was not archived)`.
//! Replays `skills/<skill>/evals/prompts.{md,yaml}` through a live agent,
//! scores each row pass/fail via an LLM judge, and writes a report.
//! `tengu skill evolve` scores through the same `run_skill`.
//!
//! | Row agent | Tools |
//! |---|---|
//! | `openrouter` / `local` | in-process executor (eval config folded like `Config::load`) → `SanitizedToolExecutor` (`process_secret_registry`) → the row's `StubbedExecutor` |
//! | `claude_code` | the MCP bridge (`bootstrap::tools::bridge_inputs`), loading the expanded eval config (`bridge_config_file`, validated first); a row with `stubs` is refused (`refuse_stubs_on_bridge`) |
//!
//! The pure parts (rows, stubs, judge, report types) are
//! `application::skills::lifecycle::eval`; `row.rs` drives one skill's rows.

mod row;
#[cfg(test)]
mod tests;

pub(crate) use crate::application::skills::lifecycle::eval::*;
pub(crate) use row::*;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::application::skills::lifecycle::metrics::{validate_metrics, MetricSpec};
use crate::config::{AgentConfig, Config};
use crate::ports::engine::Engine;

#[derive(Debug)]
pub struct EvalArgs {
    pub skills: Vec<String>,
    pub sandbox: Option<String>,
    pub judge_model: Option<String>,
    pub concurrency: usize,
    pub format: OutputFormat,
    pub out_dir: Option<PathBuf>,
    pub filter: Option<String>,
    pub keep_workspace: bool,
    /// Retention: keep only the last N run directories under the auto-generated
    /// `evals/runs/<timestamp>/` path. Older directories get deleted at startup
    /// BEFORE the current run writes anything. `None` → no cleanup (unbounded
    /// growth). Set to e.g. 10 to cap cruft.
    ///
    /// Ignored when `out_dir` is explicitly supplied — the user picked an
    /// exact path, retention cleanup of sibling dirs would be surprising.
    pub keep_runs: Option<usize>,
    /// When `true`, skip writing per-row transcripts, `report.json`, and
    /// skill `metrics.json` / `history.jsonl`. The in-memory `SkillReport`
    /// is still returned and the table / JSON summary still prints. For
    /// quick iteration without polluting the repo.
    pub no_persist: bool,
    /// Retention cap for per-skill `metrics/runs/<ts>/` directories. Passed
    /// through to `finalize_run`. `0` disables pruning. Defaults to 10.
    pub max_per_run_reports: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Json,
}

/// Delete old auto-generated run directories under `evals/runs/`, keeping
/// only the most recent `keep` by modification time. Silent no-op when the
/// parent dir doesn't exist. Non-directory entries are ignored.
fn prune_old_run_dirs(parent: &Path, keep: usize) -> std::io::Result<()> {
    if !parent.exists() {
        return Ok(());
    }
    let mut entries: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        entries.push((path, mtime));
    }
    if entries.len() <= keep {
        return Ok(());
    }
    // Newest first
    entries.sort_by(|a, b| b.1.cmp(&a.1));
    for (path, _) in entries.into_iter().skip(keep) {
        let _ = std::fs::remove_dir_all(&path);
    }
    Ok(())
}

pub async fn run(args: EvalArgs) -> anyhow::Result<i32> {
    // `[keys]` of the eval sandbox, before any engine (the judge included)
    // is built: its egress, a seal-proxy session, the exports and `strip` —
    // as `load_sandbox_or` does for every other sandbox command.
    if let Some(name) = &args.sandbox {
        let path = PathBuf::from("sandboxes").join(name).join("config.toml");
        if let Ok(cfg) = Config::load(&path) {
            if cfg.keys.enabled() {
                crate::adapters::outbound::egress::install(&cfg.egress)?;
                crate::adapters::outbound::keys::install(&cfg.keys);
            }
        }
    }
    let started_at = chrono::Utc::now();
    let auto_out_dir = args.out_dir.is_none();
    let out_dir = args.out_dir.clone().unwrap_or_else(|| {
        PathBuf::from("evals/runs").join(started_at.format("%Y-%m-%dT%H-%M-%SZ").to_string())
    });

    // Retention: prune old auto-generated run directories BEFORE creating
    // the new one. Only fires when out_dir was not explicitly supplied —
    // user-chosen paths are never touched. Default of 10 keeps recent
    // history for diagnosis without unbounded growth.
    if auto_out_dir {
        let keep = args.keep_runs.unwrap_or(10);
        if let Err(e) = prune_old_run_dirs(&PathBuf::from("evals/runs"), keep) {
            eprintln!("Warning: failed to prune old run dirs: {}", e);
        }
    }

    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("Error: create out dir {}: {}", out_dir.display(), e);
        return Ok(2);
    }

    let roots = default_skill_roots();
    let skills = match discover_skills(&args.skills, &roots) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {}", e);
            return Ok(2);
        }
    };
    if skills.is_empty() {
        eprintln!("no skills with evals/ found in roots: {:?}", roots);
        return Ok(2);
    }

    // Build the judge engine via the shared helper.
    let judge_model = args
        .judge_model
        .unwrap_or_else(|| "anthropic/claude-opus-4-7".to_string());
    let judge: Arc<dyn crate::ports::engine::Engine> = match build_judge(Some(judge_model.clone()))
    {
        Ok(j) => j,
        Err(e) => {
            eprintln!("Error: build judge engine: {}", e);
            return Ok(2);
        }
    };

    let sandbox_config = args
        .sandbox
        .as_ref()
        .map(|name| PathBuf::from("sandboxes").join(name).join("config.toml"));
    if let Some(ref p) = sandbox_config {
        if !p.exists() {
            eprintln!("Error: sandbox config not found: {}", p.display());
            return Ok(2);
        }
    }

    let mut skill_reports = Vec::new();
    let mut runner_exit = 0i32;
    for skill in &skills {
        // Runner-level errors (config parse, engine build, judge network, etc.) → exit 2.
        // Row-level timeouts produce a RowResult with timed_out=true and do NOT bubble
        // through here — they still set runner_exit = 1 via the verdict check below.
        let report = match run_skill(
            skill,
            Arc::clone(&judge),
            &out_dir,
            args.filter.as_deref(),
            args.concurrency,
            args.keep_workspace,
            sandbox_config.as_deref(),
            RunSkillOptions {
                persist: !args.no_persist,
                max_per_run_reports: args.max_per_run_reports,
            },
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Error running skill '{}': {}", skill.name, e);
                return Ok(2);
            }
        };
        if report.rows.iter().any(|r| r.verdict != "pass") {
            runner_exit = 1;
        }
        skill_reports.push(report);
    }

    let finished_at = chrono::Utc::now();

    let summary = Summary {
        total_rows: skill_reports.iter().map(|s| s.rows.len() as u32).sum(),
        passed: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .filter(|r| r.verdict == "pass")
            .count() as u32,
        failed: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .filter(|r| r.verdict != "pass")
            .count() as u32,
        timed_out: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .filter(|r| r.timed_out)
            .count() as u32,
        total_agent_tokens: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .map(|r| (r.agent_tokens.input + r.agent_tokens.output) as u64)
            .sum(),
        total_judge_tokens: skill_reports
            .iter()
            .flat_map(|s| &s.rows)
            .map(|r| (r.judge_tokens.input + r.judge_tokens.output) as u64)
            .sum(),
        wall_ms: (finished_at - started_at).num_milliseconds() as u64,
    };

    let report = Report {
        schema_version: 1,
        started_at: started_at.to_rfc3339(),
        finished_at: finished_at.to_rfc3339(),
        runner_version: format!("tengu {}", env!("CARGO_PKG_VERSION")),
        judge_model,
        concurrency: args.concurrency,
        skills: skill_reports,
        summary,
    };

    let report_path = out_dir.join("report.json");
    if let Err(e) = std::fs::write(&report_path, serde_json::to_string_pretty(&report)?) {
        eprintln!("Error: write {}: {}", report_path.display(), e);
        return Ok(2);
    }

    match args.format {
        OutputFormat::Table => print_table(&report),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
    }

    Ok(runner_exit)
}

fn should_colour() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

fn colour(s: &str, code: &str) -> String {
    if should_colour() {
        format!("\x1b[{}m{}\x1b[0m", code, s)
    } else {
        s.to_string()
    }
}

fn print_table(report: &Report) {
    for skill in &report.skills {
        println!(
            "{}  ({} rows, {:.1}s)",
            colour(&skill.skill, "1"), // bold
            skill.rows.len(),
            skill.wall_ms as f64 / 1000.0
        );
        for r in &skill.rows {
            let (mark, code) = if r.verdict == "pass" {
                ("✓", "32")
            } else {
                ("✗", "31")
            };
            println!(
                "  {} {:32} {:4}  {}",
                colour(mark, code),
                r.id,
                r.verdict,
                r.rationale
            );
            if r.verdict != "pass" {
                println!("      → see {}", r.transcript_path.display());
            }
        }
        println!();
    }
    let summary_line = format!(
        "{}/{} passed ({} failed). Total wall: {:.1}s. Agent tokens: {}. Judge tokens: {}.",
        report.summary.passed,
        report.summary.total_rows,
        report.summary.failed,
        report.summary.wall_ms as f64 / 1000.0,
        report.summary.total_agent_tokens,
        report.summary.total_judge_tokens,
    );
    if report.summary.failed == 0 {
        println!("{}", colour(&summary_line, "32"));
    } else {
        println!("{}", colour(&summary_line, "31"));
    }
}

/// Build a judge engine for eval + evolve scoring.
///
/// Called from both `inbound::eval::run` and `skill_lifecycle::evolve::run_eval_and_read_metrics`.
/// Default model: `anthropic/claude-opus-4-7`, 64 k context window.
pub fn build_judge(model: Option<String>) -> Result<Arc<dyn crate::ports::engine::Engine>> {
    let judge_model = model.unwrap_or_else(|| "anthropic/claude-opus-4-7".to_string());
    let box_engine =
        crate::adapters::outbound::engines::build_openrouter_engine(&judge_model, 64_000)?;
    Ok(Arc::from(box_engine))
}

/// The skill tiers (`config::paths::skill_tiers`) with the cwd as the
/// workspace — `tengu eval` runs outside any sandbox.
pub fn default_skill_roots() -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::config::paths::skill_tiers(&cwd)
        .into_iter()
        .map(|(_, dir)| dir)
        .collect()
}

fn tier_for_root(root: &Path) -> SkillTier {
    if root.starts_with(crate::config::paths::resolve_tengu_home().join("skills")) {
        SkillTier::Managed
    } else if root.to_string_lossy().contains("/.tengu/skills") {
        SkillTier::Workspace
    } else {
        SkillTier::Project
    }
}

pub fn discover_skills(
    filter: &[String],
    roots: &[PathBuf],
) -> anyhow::Result<Vec<SkillUnderTest>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::<String>::new(); // dedup by name; first tier wins
    for root in roots {
        if !root.exists() {
            continue;
        }
        let tier = tier_for_root(root);
        for entry in
            std::fs::read_dir(root).with_context(|| format!("read_dir {}", root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !filter.is_empty() && !filter.iter().any(|s| s == &name) {
                continue;
            }
            let evals_dir = entry.path().join("evals");
            if !evals_dir.exists() {
                continue;
            }
            let yaml = evals_dir.join("prompts.yaml");
            let md = evals_dir.join("prompts.md");
            let (prompts_path, fmt) = if yaml.exists() {
                (yaml, "yaml")
            } else if md.exists() {
                (md, "markdown")
            } else {
                continue;
            };
            // Dedup happens last, once we know this directory is a real evaluable skill.
            if !seen.insert(name.clone()) {
                continue;
            }
            let config_path = evals_dir.join("config.toml");
            let skill_dir = entry.path().to_path_buf();
            let skill_md_path = skill_dir.join("SKILL.md");
            out.push(SkillUnderTest {
                name,
                tier,
                evals_dir: evals_dir.clone(),
                prompts_path,
                prompts_format: fmt.to_string(),
                config_path,
                skill_md_path,
                skill_dir,
            });
        }
    }
    if !filter.is_empty() {
        for wanted in filter {
            if !out.iter().any(|s| &s.name == wanted) {
                anyhow::bail!("skill '{}' has no evals/prompts.{{md,yaml}}", wanted);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Skill metrics frontmatter — load + validate on skill discovery
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct SkillFrontmatter {
    #[serde(default)]
    metrics: Vec<MetricSpec>,
}

pub(crate) fn load_skill_metrics(
    skill_md_path: &Path,
    skill_dir: &Path,
) -> anyhow::Result<Vec<MetricSpec>> {
    if !skill_md_path.exists() {
        return Ok(vec![]);
    }
    let body = std::fs::read_to_string(skill_md_path)
        .with_context(|| format!("read {}", skill_md_path.display()))?;
    let Some(rest) = body.strip_prefix("---\n") else {
        return Ok(vec![]);
    };
    let Some(end) = rest.find("\n---") else {
        return Ok(vec![]);
    };
    let fm_yaml = &rest[..end];
    let fm: SkillFrontmatter = serde_yaml::from_str(fm_yaml)
        .with_context(|| format!("parse frontmatter of {}", skill_md_path.display()))?;
    if !fm.metrics.is_empty() {
        validate_metrics(&fm.metrics, skill_dir)?;
    }
    Ok(fm.metrics)
}

/// The eval config text with `{TMP_WORKSPACE}` replaced by `tmp_workspace`.
fn expanded_eval_config(path: &Path, tmp_workspace: &Path) -> anyhow::Result<String> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(raw.replace("{TMP_WORKSPACE}", &tmp_workspace.to_string_lossy()))
}

/// The eval config (skill-local `evals/config.toml` or `--sandbox`) with
/// `{TMP_WORKSPACE}` expanded. Parsed leniently (seeded configs list any
/// tool in `workspace_tools`), then folded like `Config::load` does —
/// `[default_scopes]`, sandbox sections, `no_shell_fallback` in a hardened
/// sandbox — so an eval agent's tools run under the scopes every other
/// surface applies. A `claude_code` agent also needs [`bridge_config_file`].
pub fn load_eval_config(path: &Path, tmp_workspace: &Path) -> anyhow::Result<Config> {
    let expanded = expanded_eval_config(path, tmp_workspace)?;
    let mut cfg: Config =
        toml::from_str(&expanded).with_context(|| format!("parse {}", path.display()))?;
    cfg.fold_default_scopes();
    Ok(cfg)
}

/// The agents a row may run, by name: its config's default agent (direct
/// dispatch), or every orchestrator worker — the planner gets no tools.
fn runnable_agents<'a>(
    cfg: &'a Config,
    default: (&'a str, &'a AgentConfig),
) -> Vec<(&'a str, &'a AgentConfig)> {
    let Some(orchestrator) = cfg.orchestrator.as_ref() else {
        return vec![default];
    };
    cfg.agents
        .iter()
        .filter(|(name, _)| **name != orchestrator.agent)
        .map(|(name, agent)| (name.as_str(), agent))
        .collect()
}

/// The expanded eval config as a file for a `claude_code` agent's MCP bridge
/// (`TENGU_CONFIG`), so the bridge runs tools as that `[agents.<a>]` block of
/// this config — not of the base config in the env. `None` when no agent the
/// row may run (`runnable`) is a `claude_code` one. The bridge loads it
/// strictly (`Config::load`: validated + folded; an invalid file stops the
/// bridge), so it is loaded here first and refused with the reason. Keep the
/// handle alive for the row.
fn bridge_config_file(
    path: &Path,
    tmp_workspace: &Path,
    runnable: &[(&str, &AgentConfig)],
) -> anyhow::Result<Option<tempfile::NamedTempFile>> {
    let Some((agent, _)) = runnable.iter().find(|(_, a)| a.engine == "claude_code") else {
        return Ok(None);
    };
    let mut file = tempfile::Builder::new()
        .prefix("tengu-eval-config-")
        .suffix(".toml")
        .tempfile()
        .context("create the eval config file for the MCP bridge")?;
    std::io::Write::write_all(
        &mut file,
        expanded_eval_config(path, tmp_workspace)?.as_bytes(),
    )
    .context("write the eval config file for the MCP bridge")?;
    Config::load(file.path()).with_context(|| {
        format!(
            "agent '{agent}' runs claude_code: its MCP bridge loads {} with `Config::load` \
             (validated), which refuses it",
            path.display()
        )
    })?;
    Ok(Some(file))
}

/// The engine of eval agent `agent_id`: a `claude_code` agent's MCP bridge
/// loads `bridge_config` (the eval config, [`bridge_config_file`]) and runs
/// tools as that agent; other engines ignore it.
fn build_eval_engine(
    agent_id: &str,
    agent: &AgentConfig,
    cfg: &Config,
    bridge_config: Option<&Path>,
) -> anyhow::Result<Box<dyn Engine>> {
    crate::adapters::outbound::engines::build_step_engine(
        agent_id,
        agent,
        cfg.claude_code.as_ref(),
        crate::adapters::outbound::engines::StepOpts {
            config_file: bridge_config.map(Path::to_path_buf),
            ..Default::default()
        },
    )
}

/// A row's stubs replace tool results inside this process
/// (`StubbedExecutor`). A `claude_code` agent's tools run in its MCP bridge
/// — a process the Claude CLI starts, which has no stub layer (a stub hook
/// in the production bridge would fake results wherever its env leaked) —
/// so a stubbed row on such an agent would call the real tools: refused
/// instead. `runnable` = the agents the row may run ([`runnable_agents`]).
fn refuse_stubs_on_bridge(
    row: &PromptRow,
    runnable: &[(&str, &AgentConfig)],
) -> anyhow::Result<()> {
    if row.stubs.is_empty() {
        return Ok(());
    }
    let Some((name, _)) = runnable.iter().find(|(_, a)| a.engine == "claude_code") else {
        return Ok(());
    };
    let tools: Vec<&str> = row.stubs.iter().map(|s| s.tool.as_str()).collect();
    anyhow::bail!(
        "row '{}': stubs ({}) need an engine whose tool calls run in this process — \
         agent '{name}' runs claude_code, whose tools run in its MCP bridge (no stub \
         layer); run the row on an openrouter / local agent or drop its stubs",
        row.id,
        tools.join(", ")
    )
}
