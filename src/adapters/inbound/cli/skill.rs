//! `tengu skill …` — list, doctor, install, remove, export, seed, eval, evolve.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::SkillAction;
use crate::bootstrap::sandbox::load_sandbox_or;
use crate::config::Config;

// Memory inspection: Open Brain (Postgres `agentic_memory`) is the only durable
// memory backend. Inspect it with SQL against `TENGU_MEMORY_DATABASE_URL` or
// the ignored `postgres_*_smoke` tests. A Postgres-native inspect CLI is a
// tracked follow-up in docs/SESSION_HANDOFF.md.

// ---------------------------------------------------------------------------
// `tengu skill ...` dispatcher and handlers (Batch 2 of skill-research-2026-04-28)
// ---------------------------------------------------------------------------

use crate::application::skills::lifecycle::{audit, scanner};

/// Resolve a skill directory for the given tier. `project` -> `<ws>/skills/<name>`,
/// `workspace` -> `<ws>/.tengu/skills/<name>`, `managed` -> `~/.tengu/skills/<name>`.
fn skill_dir_for_tier(workspace: &Path, tier: &str, name: &str) -> Result<PathBuf> {
    match tier {
        "project" => Ok(workspace.join("skills").join(name)),
        "workspace" => Ok(workspace.join(".tengu").join("skills").join(name)),
        "managed" => {
            let home = dirs_next::home_dir()
                .context("could not resolve home directory for managed tier")?;
            Ok(home.join(".tengu").join("skills").join(name))
        }
        other => anyhow::bail!(
            "invalid --tier '{}' (expected: project | workspace | managed)",
            other
        ),
    }
}

/// Walk all three tiers and collect (tier_label, skill_dir) pairs.
fn enumerate_all_tiers(workspace: &Path) -> Vec<(&'static str, PathBuf)> {
    let mut out = Vec::new();
    let project = workspace.join("skills");
    if project.is_dir() {
        if let Ok(rd) = std::fs::read_dir(&project) {
            for e in rd.flatten() {
                if e.path().join("SKILL.md").is_file() {
                    out.push(("project", e.path()));
                }
            }
        }
    }
    let ws = workspace.join(".tengu").join("skills");
    if ws.is_dir() {
        if let Ok(rd) = std::fs::read_dir(&ws) {
            for e in rd.flatten() {
                if e.path().join("SKILL.md").is_file() {
                    out.push(("workspace", e.path()));
                }
            }
        }
    }
    if let Some(home) = dirs_next::home_dir() {
        let managed = home.join(".tengu").join("skills");
        if managed.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&managed) {
                for e in rd.flatten() {
                    if e.path().join("SKILL.md").is_file() {
                        out.push(("managed", e.path()));
                    }
                }
            }
        }
    }
    out
}

/// Minimal frontmatter view used by list/export/install. Captures only the
/// fields these verbs need; deserialisation is forgiving (missing fields → None).
#[derive(Debug, serde::Deserialize, Default)]
struct SkillFrontmatterLite {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    /// Raw `metrics:` array — left as serde_yaml::Value so we can scan for
    /// `kind: script` / `kind: shell_check` without depending on the full
    /// `MetricSpec` enum. Keeps parsing fail-soft when fields differ.
    #[serde(default)]
    metrics: Option<serde_yaml::Value>,
}

fn read_frontmatter_lite(skill_md: &Path) -> Option<SkillFrontmatterLite> {
    let content = std::fs::read_to_string(skill_md).ok()?;
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    let end = rest.find("\n---")?;
    let yaml_text = rest[..end].trim_start_matches('\n');
    serde_yaml::from_str::<SkillFrontmatterLite>(yaml_text).ok()
}

/// True if the frontmatter declares any `Script` or `ShellCheck` metric kinds.
fn declares_shell_kind(fm: &SkillFrontmatterLite, kinds: &[&str]) -> bool {
    let arr = match &fm.metrics {
        Some(serde_yaml::Value::Sequence(s)) => s,
        _ => return false,
    };
    arr.iter().any(|m| {
        m.get("kind")
            .and_then(|v| v.as_str())
            .map(|k| kinds.contains(&k))
            .unwrap_or(false)
    })
}

pub(super) async fn run_skill_command(config: Config, action: SkillAction) -> Result<()> {
    match action {
        SkillAction::Evolve {
            skill,
            max_cycles,
            target_metric,
            base_branch,
            sandbox,
        } => {
            let config = load_sandbox_or(sandbox, config)?;
            let workspace = std::env::current_dir()?;
            let chat_factory =
                crate::bootstrap::orchestrator::build_cli_chat_factory(&config, &workspace).await?;
            let args = crate::adapters::inbound::evolve::EvolveArgs {
                config: &config,
                workspace: &workspace,
                skill: &skill,
                max_cycles,
                target_metric,
                base_branch,
                chat_factory,
            };
            crate::adapters::inbound::evolve::run_evolve(args).await?;
            Ok(())
        }
        SkillAction::Metrics { skill, last } => {
            let workspace = std::env::current_dir()?;
            let skill_dir = workspace.join("skills").join(&skill);
            let mj_path = skill_dir.join("metrics.json");
            if !mj_path.exists() {
                eprintln!(
                    "No metrics.json yet for skill '{}'. Run `tengu eval {}` first.",
                    skill, skill
                );
                return Ok(());
            }
            let raw = std::fs::read(&mj_path)?;
            let v: serde_json::Value = serde_json::from_slice(&raw)?;
            println!("{}", serde_json::to_string_pretty(&v)?);

            // Friendly per-metric summary with variance band when available.
            if let Ok(mj) = serde_json::from_slice::<
                crate::application::skills::lifecycle::storage::MetricsJson,
            >(&raw)
            {
                println!("\n-- summary --");
                for (name, r) in &mj.metrics {
                    let band = match (r.stddev, r.min, r.max) {
                        (Some(sd), Some(mn), Some(mx)) => {
                            format!(" ± {:.2} [{:.2}–{:.2}]", sd, mn, mx)
                        }
                        _ => String::new(),
                    };
                    let gated = if r.gated { " [GATED]" } else { "" };
                    println!("{}: {:.2}{}  n={}{}", name, r.pass_rate, band, r.n, gated);
                }
            }

            let hpath = skill_dir.join("metrics").join("history.jsonl");
            if hpath.exists() {
                println!("\n-- history (last {last}) --");
                let text = std::fs::read_to_string(&hpath)?;
                let lines: Vec<&str> = text.lines().collect();
                for l in lines.iter().rev().take(last as usize).rev() {
                    println!("{l}");
                }
            }
            Ok(())
        }
        SkillAction::AcceptProposal { path } => {
            eprintln!(
                "accept-proposal is a placeholder in v1. \
                 Proposals are applied inline during `tengu skill evolve`. \
                 Path ignored: {}",
                path.display()
            );
            Ok(())
        }
        SkillAction::Remove { name, tier, yes } => skill_remove(&name, &tier, yes).await,
        SkillAction::List { tier } => skill_list(tier.as_deref()).await,
        SkillAction::Doctor { sandbox, no_fail } => {
            let config = load_sandbox_or(sandbox, config)?;
            skill_doctor(&config, no_fail).await
        }
        SkillAction::Export { name, out } => skill_export(&name, out.as_deref()).await,
        SkillAction::Install {
            source,
            tier,
            strict,
            yes,
        } => skill_install(&source, &tier, strict, yes).await,
        SkillAction::Seed {
            name,
            resources_dir,
            tier,
            description,
            learner_facing,
            yes,
        } => {
            skill_seed(
                &name,
                resources_dir.as_deref(),
                &tier,
                description.as_deref(),
                learner_facing,
                yes,
            )
            .await
        }
    }
}

/// `tengu skill remove` — delete `<tier>/<name>/`, refusing if an active
/// evolve worktree exists. Atomic via `std::fs::remove_dir_all` and
/// audit-logged.
async fn skill_remove(name: &str, tier: &str, yes: bool) -> Result<()> {
    let workspace = std::env::current_dir()?;
    let skill_dir = skill_dir_for_tier(&workspace, tier, name)?;
    if !skill_dir.is_dir() {
        anyhow::bail!(
            "no skill '{}' at {} (tier={})",
            name,
            skill_dir.display(),
            tier
        );
    }

    // Refuse if an active evolve worktree exists (matches the scratch_worktree
    // naming pattern: <ws>/.tengu/worktrees/evolve-<name>-*).
    let worktree_root = workspace.join(".tengu").join("worktrees");
    if worktree_root.is_dir() {
        if let Ok(rd) = std::fs::read_dir(&worktree_root) {
            for e in rd.flatten() {
                let fname = e.file_name();
                let s = fname.to_string_lossy();
                if s.starts_with(&format!("evolve-{}-", name)) {
                    anyhow::bail!(
                        "active evolve worktree {} blocks remove; finish or sweep it first",
                        e.path().display()
                    );
                }
            }
        }
    }

    // Print artefact summary BEFORE prompting — operator decides with full info.
    let evals_dir = skill_dir.join("evals");
    let fixture_count = if evals_dir.is_dir() {
        std::fs::read_dir(&evals_dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("yaml"))
                    .count()
            })
            .unwrap_or(0)
    } else {
        0
    };
    let metrics_json_present = skill_dir.join("metrics.json").is_file();
    let runs_count = {
        let runs = skill_dir.join("metrics").join("runs");
        if runs.is_dir() {
            std::fs::read_dir(&runs)
                .map(|rd| rd.flatten().count())
                .unwrap_or(0)
        } else {
            0
        }
    };
    println!(
        "About to remove {} (tier={})\n  fixtures: {}\n  metrics.json: {}\n  runs/: {}",
        skill_dir.display(),
        tier,
        fixture_count,
        if metrics_json_present { "yes" } else { "no" },
        runs_count
    );

    if !yes {
        eprint!("Proceed? [y/N] ");
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        if !buf.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    std::fs::remove_dir_all(&skill_dir)
        .with_context(|| format!("remove {}", skill_dir.display()))?;
    println!("removed {}", skill_dir.display());

    let entry = audit::AuditEntry {
        ts: chrono::Utc::now().to_rfc3339(),
        op: "remove".to_string(),
        name: name.to_string(),
        verdict: None,
        source: Some(format!("tier={}", tier)),
        sha256: None,
    };
    if let Err(e) = audit::append(&workspace, entry) {
        tracing::warn!(error = %e, "audit append failed (non-fatal)");
    }
    Ok(())
}

/// `tengu skill list` — walk all three tiers, print a compact table.
async fn skill_list(tier_filter: Option<&str>) -> Result<()> {
    let workspace = std::env::current_dir()?;
    let entries = enumerate_all_tiers(&workspace);
    let mut rows: Vec<(String, &'static str, usize, String, String, bool)> = Vec::new();
    for (tier_label, dir) in entries {
        if let Some(t) = tier_filter {
            if t != tier_label {
                continue;
            }
        }
        let skill_md = dir.join("SKILL.md");
        let fm = read_frontmatter_lite(&skill_md).unwrap_or_default();
        let name = fm.name.clone().unwrap_or_else(|| {
            dir.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string()
        });

        let fixture_count = {
            let evals = dir.join("evals");
            if evals.is_dir() {
                std::fs::read_dir(&evals)
                    .map(|rd| {
                        rd.flatten()
                            .filter(|e| {
                                e.path().extension().and_then(|s| s.to_str()) == Some("yaml")
                            })
                            .count()
                    })
                    .unwrap_or(0)
            } else {
                0
            }
        };

        // Gated-metrics column: pull `min_pass_rate`-bearing metric names from
        // metrics.json if present. Cheap signal; no scoring here.
        let mj = dir.join("metrics.json");
        let gated = if mj.is_file() {
            std::fs::read_to_string(&mj)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| {
                    v.get("metrics")
                        .and_then(|m| m.as_object())
                        .map(|o| o.keys().cloned().collect::<Vec<_>>().join(","))
                })
                .unwrap_or_default()
        } else {
            String::new()
        };

        let last_run = mj
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| {
                chrono::DateTime::<chrono::Utc>::from_timestamp(d.as_secs() as i64, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
            })
            .unwrap_or_else(|| "-".to_string());

        let shell_marked = declares_shell_kind(&fm, &["script", "shell_check"]);
        rows.push((
            name,
            tier_label,
            fixture_count,
            gated,
            last_run,
            shell_marked,
        ));
    }

    rows.sort_by(|a, b| a.0.cmp(&b.0));
    println!(
        "{:<28} {:<10} {:>8}  {:<24}  {:<25}",
        "name", "tier", "fixtures", "gated metrics", "last_run"
    );
    println!("{}", "-".repeat(100));
    for (name, tier, fixtures, gated, last_run, shell) in rows {
        let glyph = if shell { " (warn) " } else { "" };
        println!(
            "{:<28} {:<10} {:>8}  {:<24}  {:<25}{}",
            name, tier, fixtures, gated, last_run, glyph
        );
    }
    Ok(())
}

/// `tengu skill doctor` — cross-check `[agents.*].skill_packages` vs filesystem.
async fn skill_doctor(config: &Config, no_fail: bool) -> Result<()> {
    let workspace = std::env::current_dir()?;

    // Collect all installed skill names across the three tiers.
    let installed = enumerate_all_tiers(&workspace);
    let installed_names: std::collections::HashSet<String> = installed
        .iter()
        .map(|(_, d)| {
            read_frontmatter_lite(&d.join("SKILL.md"))
                .and_then(|fm| fm.name)
                .unwrap_or_else(|| {
                    d.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("?")
                        .to_string()
                })
        })
        .collect();

    // Skills referenced by the `[agents.*]` blocks of the active config.
    let mut referenced: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (agent_name, agent) in &config.agents {
        for s in &agent.skill_packages {
            referenced
                .entry(s.clone())
                .or_default()
                .push(agent_name.clone());
        }
    }

    // Phantoms = agent refs with no skill on disk.
    let mut phantoms: Vec<(String, Vec<String>)> = referenced
        .iter()
        .filter(|(s, _)| !installed_names.contains(*s))
        .map(|(s, agents)| (s.clone(), agents.clone()))
        .collect();
    phantoms.sort();

    // Orphans = installed skills with no agent ref.
    let mut orphans: Vec<String> = installed_names
        .iter()
        .filter(|s| !referenced.contains_key(*s))
        .cloned()
        .collect();
    orphans.sort();

    println!("# tengu skill doctor");
    println!();
    println!(
        "phantoms ({}): agent refs with no skill on disk",
        phantoms.len()
    );
    for (s, agents) in &phantoms {
        println!("  {} <- {}", s, agents.join(","));
    }
    println!();
    println!(
        "orphans ({}): installed skills with no agent ref",
        orphans.len()
    );
    for s in &orphans {
        println!("  {}", s);
    }

    // Missing rubric files: walk metrics.json + frontmatter `metrics:` and
    // collect LlmJudge.rubric_file refs that point outside the skill_dir.
    println!();
    println!("missing rubric files:");
    let mut missing_rubrics: Vec<String> = Vec::new();
    for (_tier, dir) in &installed {
        let fm = match read_frontmatter_lite(&dir.join("SKILL.md")) {
            Some(f) => f,
            None => continue,
        };
        let arr = match fm.metrics {
            Some(serde_yaml::Value::Sequence(s)) => s,
            _ => continue,
        };
        for m in arr {
            if m.get("kind").and_then(|v| v.as_str()) != Some("llm_judge") {
                continue;
            }
            let rf = match m.get("rubric_file").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => continue,
            };
            let abs = dir.join(rf);
            if !abs.is_file() {
                missing_rubrics.push(format!("{} :: {}", dir.display(), rf));
            }
        }
    }
    if missing_rubrics.is_empty() {
        println!("  (none)");
    } else {
        for m in &missing_rubrics {
            println!("  {}", m);
        }
    }

    // Scanner findings (informational).
    println!();
    println!("scanner findings (informational):");
    for (_tier, dir) in &installed {
        match scanner::scan_skill(dir) {
            Ok(result) => {
                if !result.findings.is_empty() {
                    println!("{}", scanner::render_findings_table(&result));
                }
            }
            Err(e) => {
                tracing::warn!(skill = %dir.display(), error = %e, "scanner failed");
            }
        }
    }

    if !phantoms.is_empty() && !no_fail {
        std::process::exit(1);
    }
    Ok(())
}

/// `tengu skill export` — tar.gz of SKILL.md, evals/prompts.yaml, and
/// metrics/*.md. metrics/*.sh included only if frontmatter declares any
/// `Script` metrics. We shell out to `tar`, `flate2`/`tar` crates aren't deps.
async fn skill_export(name: &str, out: Option<&Path>) -> Result<()> {
    let workspace = std::env::current_dir()?;
    let skill_dir = workspace.join("skills").join(name);
    if !skill_dir.is_dir() {
        anyhow::bail!("no skill '{}' at {}", name, skill_dir.display());
    }

    let fm = read_frontmatter_lite(&skill_dir.join("SKILL.md")).unwrap_or_default();
    let include_scripts = declares_shell_kind(&fm, &["script"]);

    // Stage files in a tempdir so `tar` only sees what we want.
    let staging = tempfile::tempdir().context("create staging tempdir")?;
    let stage_root = staging.path().join(name);
    std::fs::create_dir_all(&stage_root)?;

    // Always include SKILL.md.
    let src_md = skill_dir.join("SKILL.md");
    if src_md.is_file() {
        std::fs::copy(&src_md, stage_root.join("SKILL.md"))?;
    }
    // evals/prompts.yaml only.
    let prompts = skill_dir.join("evals").join("prompts.yaml");
    if prompts.is_file() {
        std::fs::create_dir_all(stage_root.join("evals"))?;
        std::fs::copy(&prompts, stage_root.join("evals").join("prompts.yaml"))?;
    }
    // metrics/*.md (rubric files) — skip metrics.json + runs/ + history.jsonl.
    let metrics_dir = skill_dir.join("metrics");
    if metrics_dir.is_dir() {
        let dst = stage_root.join("metrics");
        std::fs::create_dir_all(&dst)?;
        for e in std::fs::read_dir(&metrics_dir)?.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
            let keep = ext == "md" || (include_scripts && ext == "sh");
            if !keep {
                continue;
            }
            if let Some(fname) = p.file_name() {
                std::fs::copy(&p, dst.join(fname))?;
            }
        }
    }

    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let out_path = out
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| workspace.join(format!("{}-{}.tar.gz", name, ts)));

    // Shell out to `tar -czf <out> -C <staging> <name>`. `tar` crate isn't a
    // dep; system tar is fine for v1 (Linux + macOS both ship one).
    let shell = crate::adapters::outbound::shell::LocalShellExecutor::new();
    use crate::ports::shell::ShellExecutionPort;
    let cmd = format!(
        "tar -czf {} -C {} {}",
        shell_quote(&out_path.to_string_lossy()),
        shell_quote(&staging.path().to_string_lossy()),
        shell_quote(name)
    );
    shell
        .execute_shell(&cmd, &workspace)
        .with_context(|| format!("tar -czf failed: {}", cmd))?;

    println!("wrote {}", out_path.display());

    let entry = audit::AuditEntry {
        ts: chrono::Utc::now().to_rfc3339(),
        op: "export".to_string(),
        name: name.to_string(),
        verdict: None,
        source: Some(out_path.to_string_lossy().into_owned()),
        sha256: None,
    };
    if let Err(e) = audit::append(&workspace, entry) {
        tracing::warn!(error = %e, "audit append failed (non-fatal)");
    }
    Ok(())
}

fn shell_quote(s: &str) -> String {
    // Conservative single-quote escaping good enough for path args.
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `tengu skill install` — quarantine -> extract -> symlink-aware validate ->
/// scan -> atomic move -> audit.
async fn skill_install(source: &str, tier: &str, strict: bool, yes: bool) -> Result<()> {
    let workspace = std::env::current_dir()?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let quarantine_root = workspace
        .join(".tengu")
        .join("quarantine")
        .join(format!("install-{}", nanos));
    std::fs::create_dir_all(&quarantine_root)
        .with_context(|| format!("create quarantine {}", quarantine_root.display()))?;

    let shell = crate::adapters::outbound::shell::LocalShellExecutor::new();
    use crate::ports::shell::ShellExecutionPort;

    // Step 2: bring source into quarantine_root.
    let is_git = source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@");
    let is_tarball = source.ends_with(".tar.gz") || source.ends_with(".tgz");

    let bring_in_result: Result<()> = if is_tarball {
        // Local-or-URL tarball. If URL, fetch via curl into quarantine first.
        let local_tarball = if source.starts_with("http://") || source.starts_with("https://") {
            let dst = quarantine_root.join("source.tar.gz");
            let cmd = format!(
                "curl -sSfL {} -o {}",
                shell_quote(source),
                shell_quote(&dst.to_string_lossy())
            );
            shell.execute_shell(&cmd, &workspace)?;
            dst
        } else {
            PathBuf::from(source)
        };
        let cmd = format!(
            "tar -xzf {} -C {}",
            shell_quote(&local_tarball.to_string_lossy()),
            shell_quote(&quarantine_root.to_string_lossy())
        );
        shell.execute_shell(&cmd, &workspace)?;
        Ok(())
    } else if is_git {
        let cmd = format!(
            "git clone {} {}",
            shell_quote(source),
            shell_quote(&quarantine_root.to_string_lossy())
        );
        shell.execute_shell(&cmd, &workspace)?;
        Ok(())
    } else {
        // Local directory copy.
        let src_dir = PathBuf::from(source);
        if !src_dir.is_dir() {
            anyhow::bail!("local source '{}' is not a directory", source);
        }
        copy_dir_recursive(&src_dir, &quarantine_root)?;
        Ok(())
    };

    if let Err(e) = bring_in_result {
        let _ = std::fs::remove_dir_all(&quarantine_root);
        return Err(e.context("fetch / extract source into quarantine"));
    }

    // Step 3: symlink-aware extraction check. Walk every entry, canonicalize,
    // assert it stays inside quarantine_root.
    let q_canon =
        std::fs::canonicalize(&quarantine_root).context("canonicalize quarantine root")?;
    if let Err(e) = assert_no_escape(&quarantine_root, &q_canon) {
        let _ = std::fs::remove_dir_all(&quarantine_root);
        eprintln!("install rejected: {}", e);
        std::process::exit(1);
    }

    // Step 4: locate SKILL.md (top-level OR under exactly one subdir like a
    // git checkout). Validate frontmatter parses with non-empty name + desc.
    let skill_root = locate_skill_root(&quarantine_root)
        .ok_or_else(|| anyhow::anyhow!("no SKILL.md found in source"))?;
    let fm = read_frontmatter_lite(&skill_root.join("SKILL.md"))
        .ok_or_else(|| anyhow::anyhow!("SKILL.md missing or malformed frontmatter"))?;
    let skill_name = fm
        .name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("SKILL.md frontmatter `name` is empty"))?;
    if fm
        .description
        .as_deref()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
    {
        anyhow::bail!("SKILL.md frontmatter `description` is empty");
    }

    // Step 5: scan, always print findings.
    let scan = scanner::scan_skill(&skill_root)?;
    println!("{}", scanner::render_findings_table(&scan));

    // Step 6: --strict gate.
    let verdict_str = match scan.verdict {
        scanner::Verdict::Safe => "safe",
        scanner::Verdict::Caution => "caution",
        scanner::Verdict::Dangerous => "dangerous",
    };
    if strict
        && matches!(
            scan.verdict,
            scanner::Verdict::Caution | scanner::Verdict::Dangerous
        )
    {
        let _ = std::fs::remove_dir_all(&quarantine_root);
        eprintln!(
            "install refused: --strict and verdict={} (use without --strict to proceed)",
            verdict_str
        );
        std::process::exit(1);
    }
    if !yes
        && matches!(
            scan.verdict,
            scanner::Verdict::Caution | scanner::Verdict::Dangerous
        )
    {
        eprint!("verdict={}; proceed? [y/N] ", verdict_str);
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        if !buf.trim().eq_ignore_ascii_case("y") {
            let _ = std::fs::remove_dir_all(&quarantine_root);
            println!("Aborted.");
            return Ok(());
        }
    }

    // Step 7: atomic move quarantine -> install root.
    let install_dir = skill_dir_for_tier(&workspace, tier, &skill_name)?;
    if let Some(parent) = install_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if install_dir.exists() {
        let _ = std::fs::remove_dir_all(&quarantine_root);
        anyhow::bail!(
            "install target {} already exists; remove it first",
            install_dir.display()
        );
    }
    std::fs::rename(&skill_root, &install_dir).with_context(|| {
        format!(
            "atomic move {} -> {}",
            skill_root.display(),
            install_dir.display()
        )
    })?;
    // Best-effort cleanup of quarantine dir (may be empty after rename, may
    // contain leftover non-skill files like a clone's `.git`).
    let _ = std::fs::remove_dir_all(&quarantine_root);

    println!("installed {} -> {}", skill_name, install_dir.display());

    // Step 8: audit. sha256 over SKILL.md content.
    let skill_md_bytes = std::fs::read(install_dir.join("SKILL.md")).unwrap_or_default();
    use sha2::{Digest, Sha256};
    let sha = format!("{:x}", Sha256::digest(&skill_md_bytes));
    let entry = audit::AuditEntry {
        ts: chrono::Utc::now().to_rfc3339(),
        op: "install".to_string(),
        name: skill_name,
        verdict: Some(verdict_str.to_string()),
        source: Some(source.to_string()),
        sha256: Some(sha),
    };
    if let Err(e) = audit::append(&workspace, entry) {
        tracing::warn!(error = %e, "audit append failed (non-fatal)");
    }
    Ok(())
}

/// `tengu skill seed` — teacher onboarding. Drops a SKILL.md template and
/// copies a folder of teacher-provided materials into
/// `<tier>/<name>/resources/`. Atomic via temp-dir + rename, mirroring
/// `skill_distill` (`src/adapters/outbound/tools/skill_lifecycle/distill.rs:147–204`).
async fn skill_seed(
    name: &str,
    resources_dir: Option<&Path>,
    tier: &str,
    description: Option<&str>,
    learner_facing: bool,
    yes: bool,
) -> Result<()> {
    // Same name regex as skill_distill — kebab-case, ^[a-z][a-z0-9-]{1,63}$.
    let re = regex::Regex::new("^[a-z][a-z0-9-]{1,63}$").unwrap();
    if !re.is_match(name) {
        anyhow::bail!(
            "invalid skill name '{}': expected kebab-case, ^[a-z][a-z0-9-]{{1,63}}$",
            name
        );
    }

    if let Some(rd) = resources_dir {
        if !rd.exists() {
            anyhow::bail!("resources_dir does not exist: {}", rd.display());
        }
        if !rd.is_dir() {
            anyhow::bail!("resources_dir is not a directory: {}", rd.display());
        }
    }

    let workspace = std::env::current_dir()?;
    let skill_dir = skill_dir_for_tier(&workspace, tier, name)?;
    if skill_dir.exists() {
        anyhow::bail!(
            "destination already exists: {} — run `tengu skill remove {} --tier {}` first if intentional",
            skill_dir.display(),
            name,
            tier
        );
    }
    let tier_root = skill_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("skill_dir {} has no parent", skill_dir.display()))?
        .to_path_buf();
    std::fs::create_dir_all(&tier_root)
        .with_context(|| format!("create tier root {}", tier_root.display()))?;

    let description = description.map(|s| s.to_string()).unwrap_or_else(|| {
        "Use when the learner needs help with topics covered by this skill's resources/ folder."
            .to_string()
    });

    // Pre-flight: count files we'll copy so the summary printed before the
    // (optional) prompt is accurate.
    let resource_count = match resources_dir {
        Some(rd) => count_files_skipping_dotfiles(rd)?,
        None => 0,
    };

    let resources_summary = match resources_dir {
        Some(rd) => format!("{} file(s) from {}", resource_count, rd.display()),
        None => "(none — empty resources/ will be created)".to_string(),
    };
    println!(
        "About to seed skill '{}' (tier={})\n  destination: {}\n  resources: {}\n  learner_facing: {}\n  editable_by_learner: {}",
        name,
        tier,
        skill_dir.display(),
        resources_summary,
        learner_facing,
        learner_facing,
    );

    if !yes {
        eprint!("Proceed? [y/N] ");
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        if !buf.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Atomic write via tempdir + rename — same pattern as
    // `src/adapters/outbound/tools/skill_lifecycle/distill.rs:148`.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = tier_root.join(format!(".{}.tmp-{}", name, nanos));
    std::fs::create_dir_all(&tmp).with_context(|| format!("create tmp {}", tmp.display()))?;

    // Cleanup-on-drop for the tmp dir if anything below errors before rename.
    let mut cleanup = TmpDirGuard {
        path: Some(tmp.clone()),
    };

    // Generate SKILL.md from the template.
    let title = title_case_from_kebab(name);
    let skill_md = format!(
        "---\nname: {name}\ndescription: {description}\neditable_by_learner: {flag}\nlearner_facing: {flag}\n---\n\n# {title}\n\nA teacher-seeded skill. Reference materials live under `skills/{name}/resources/`.\nThis directory is NOT in the agent's tmp workspace — read it via the\n`skill_resource` tool, not via `read_file`.\n\n## When to Use\n\n- The learner asks about topics covered by this skill.\n- The user types `adjust yourself` to refresh the skill against their\n  current weak areas.\n\n## How to read the resources\n\nThe `skill_resource` tool walks managed → workspace → project tiers and\nfinds this skill's resources/ folder regardless of where the agent is\nrunning. ALWAYS use it; never use `read_file` against `skills/...`.\n\n```\nskill_resource(action=\"list\", skill=\"{name}\")\n  → {{files: [{{path, size_bytes}}], count}}\n\nskill_resource(action=\"read\", skill=\"{name}\", path=\"<file>\")\n  → {{content, bytes}}\n```\n\n## Procedure\n\n1. Call `skill_resource(action=\"list\", skill=\"{name}\")` to inventory what\n   materials exist. If the result has `count: 0`, tell the learner the\n   skill has no resources yet and suggest `adjust yourself` to populate.\n2. For each learner question, pick the most relevant entry from the list,\n   then call `skill_resource(action=\"read\", skill=\"{name}\", path=\"<file>\")`\n   to fetch its content. Cite or summarise from there.\n3. Don't invent material that isn't in the resources. If you can't find a\n   relevant resource, say so and suggest `adjust yourself`.\n\n## Common Mistakes\n\n- Using `read_file` for skill resources — the agent's workspace doesn't\n  see them. Always `skill_resource`.\n- Citing material the resources don't actually contain (hallucination).\n- Drilling on a topic the learner already mastered (read state.json).\n- Adding new resource files without going through `resource-finder` —\n  the curator step exists for a reason.\n",
        name = name,
        description = description,
        flag = learner_facing,
        title = title,
    );
    std::fs::write(tmp.join("SKILL.md"), skill_md)
        .with_context(|| format!("write SKILL.md in {}", tmp.display()))?;

    // Copy the resources directory recursively into <tmp>/resources/, preserving
    // subdirectory structure and skipping dotfiles. When resources_dir is
    // None, just create the empty directory + a tiny README explaining how
    // resources accrue.
    let resources_dst = tmp.join("resources");
    std::fs::create_dir_all(&resources_dst)?;
    let copied = match resources_dir {
        Some(rd) => copy_resources_skipping_dotfiles(rd, &resources_dst)?,
        None => {
            std::fs::write(
                resources_dst.join("README.md"),
                "# Resources\n\n\
                 This folder holds the skill's reference material — markdown notes, \
                 web links, PDFs, etc. Agents read these via the `skill_resource` \
                 tool (not `read_file` — the agent's workspace is a tmp dir and \
                 doesn't see this path).\n\n\
                 Three ways to populate it:\n\n\
                 1. Drop files into this directory directly.\n\
                 2. Re-run `tengu skill seed <name> <dir>` against a different \
                    skill name (this skill is already seeded).\n\
                 3. In a chat session, type `adjust yourself` — the \
                    `resource-finder` agent fetches relevant web sources and the \
                    `skill-improver-inline` agent commits them here under an \
                    approval gate.\n",
            )?;
            0
        }
    };

    // Stub evals/prompts.yaml — schema_version: 1, one placeholder fixture.
    std::fs::create_dir_all(tmp.join("evals"))?;
    let stub = crate::application::skills::lifecycle::fixtures::FixturesFile {
        schema_version: 1,
        fixtures: vec![crate::application::skills::lifecycle::fixtures::Fixture {
            id: "f1".to_string(),
            prompt: "<TODO: a typical question a learner would ask>".to_string(),
            expected_tool_calls: Vec::new(),
            expected_outcome: Some(String::new()),
            metrics: Vec::new(),
        }],
    };
    crate::application::skills::lifecycle::fixtures::write_fixtures(
        &tmp.join("evals").join("prompts.yaml"),
        &stub,
    )?;

    // Atomic rename — last step. Once this succeeds, disarm the cleanup guard.
    std::fs::rename(&tmp, &skill_dir)
        .with_context(|| format!("rename {} -> {}", tmp.display(), skill_dir.display()))?;
    cleanup.path = None;

    println!(
        "seeded skill '{}' (tier={})\n  path:               {}\n  resources copied:   {}\n  frontmatter keys:   name, description, editable_by_learner={}, learner_facing={}",
        name,
        tier,
        skill_dir.display(),
        copied,
        learner_facing,
        learner_facing,
    );

    let entry = audit::AuditEntry {
        ts: chrono::Utc::now().to_rfc3339(),
        op: "seed".to_string(),
        name: name.to_string(),
        verdict: None,
        source: Some(format!(
            "tier={};resources_dir={}",
            tier,
            resources_dir
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<none>".to_string())
        )),
        sha256: None,
    };
    if let Err(e) = audit::append(&workspace, entry) {
        tracing::warn!(error = %e, "audit append failed (non-fatal)");
    }
    Ok(())
}

/// RAII guard: best-effort `remove_dir_all` of a tmp dir on drop. Disarmed
/// by setting `path = None` after a successful atomic rename.
struct TmpDirGuard {
    path: Option<PathBuf>,
}

impl Drop for TmpDirGuard {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

/// Convert "german-teacher" -> "German Teacher" for the SKILL.md heading.
fn title_case_from_kebab(s: &str) -> String {
    s.split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                Some(c) => c.to_uppercase().chain(cs).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Recursively count files in `src`, skipping any whose filename starts with
/// `.`. Subdirectories whose own name starts with `.` are skipped entirely.
fn count_files_skipping_dotfiles(src: &Path) -> Result<usize> {
    let mut total = 0usize;
    for e in std::fs::read_dir(src)?.flatten() {
        let name = e.file_name();
        let s = name.to_string_lossy();
        if s.starts_with('.') {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            total += count_files_skipping_dotfiles(&p)?;
        } else {
            total += 1;
        }
    }
    Ok(total)
}

/// Recursively copy `src` -> `dst`, preserving subdirectory structure and
/// skipping any entry whose filename starts with `.`. Returns the number of
/// files copied.
fn copy_resources_skipping_dotfiles(src: &Path, dst: &Path) -> Result<usize> {
    if !dst.exists() {
        std::fs::create_dir_all(dst)?;
    }
    let mut copied = 0usize;
    for e in std::fs::read_dir(src)?.flatten() {
        let name = e.file_name();
        let s = name.to_string_lossy();
        if s.starts_with('.') {
            continue;
        }
        let p = e.path();
        let target = dst.join(&name);
        if p.is_dir() {
            copied += copy_resources_skipping_dotfiles(&p, &target)?;
        } else {
            std::fs::copy(&p, &target)
                .with_context(|| format!("copy {} -> {}", p.display(), target.display()))?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// Recursively copy a directory tree — used by the local-path branch of
/// `skill install`. Symlinks are followed via `fs::copy` (the canonicalize
/// pass after extraction will reject any that escape the quarantine).
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    if !dst.exists() {
        std::fs::create_dir_all(dst)?;
    }
    for e in std::fs::read_dir(src)?.flatten() {
        let p = e.path();
        let target = dst.join(e.file_name());
        if p.is_dir() {
            copy_dir_recursive(&p, &target)?;
        } else {
            std::fs::copy(&p, &target)?;
        }
    }
    Ok(())
}

/// Walk the quarantine tree and assert every canonicalized path stays under
/// `q_canon`. Catches symlink-escape attacks (the Hermes pattern).
fn assert_no_escape(root: &Path, q_canon: &Path) -> Result<()> {
    for e in std::fs::read_dir(root)?.flatten() {
        let p = e.path();
        let canon =
            std::fs::canonicalize(&p).with_context(|| format!("canonicalize {}", p.display()))?;
        if !canon.starts_with(q_canon) {
            anyhow::bail!(
                "path {} escapes quarantine ({} -> {})",
                p.display(),
                p.display(),
                canon.display()
            );
        }
        if p.is_dir() && !p.is_symlink() {
            assert_no_escape(&p, q_canon)?;
        }
    }
    Ok(())
}

/// Find the directory containing `SKILL.md`. Either the quarantine root
/// itself or — common after `git clone` — a single subdirectory.
fn locate_skill_root(quarantine: &Path) -> Option<PathBuf> {
    if quarantine.join("SKILL.md").is_file() {
        return Some(quarantine.to_path_buf());
    }
    // Try first-level subdirs (not recursive — keeps install policy tight).
    for e in std::fs::read_dir(quarantine).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() && p.join("SKILL.md").is_file() {
            return Some(p);
        }
    }
    None
}
