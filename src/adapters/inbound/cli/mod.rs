//! `tengu` CLI — clap definitions and command dispatch. `main.rs` only calls
//! [`run`]. Subcommands with real bodies live beside this file.

mod doctor;
mod run_agent;
mod skill;

use clap::{Parser, Subcommand};

use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::info;

use crate::adapters::outbound::secrets;
use crate::bootstrap::sandbox::load_sandbox_or;
use crate::config::paths::{default_config_path, resolve_tengu_home};
use crate::config::{Config, RuntimeProfile};
use crate::domain::secrets::SecretRegistry;
use doctor::{print_status, run_doctor};
use run_agent::run_agent_subprocess;
use skill::run_skill_command;

#[derive(Parser)]
#[command(name = "tengu")]
#[command(about = "Model-agnostic AI chat. Single binary, zero dependencies.")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run interactive chat loop.
    Chat {
        /// Load config from sandboxes/<name>/config.toml instead of ~/.tengu/config.toml
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// Print static runtime status snapshot.
    Status,
    /// Run runtime/environment diagnostics.
    Doctor {
        /// Load config from sandboxes/<name>/config.toml instead of ~/.tengu/config.toml
        #[arg(long)]
        sandbox: Option<String>,
        /// Also send a live request through the `[egress]` proxy to
        /// check.torproject.org and fail unless it reports a Tor exit.
        #[arg(long)]
        tor: bool,
    },
    /// Run Telegram bot adapter.
    Telegram {
        /// Load config from sandboxes/<name>/config.toml instead of ~/.tengu/config.toml
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// Run the inbound webhook listener (`[webhooks.endpoints.<name>]` blocks
    /// in the sandbox config bind URL paths to agents). Returns 202 Accepted
    /// on every authenticated POST and dispatches a one-shot orchestrator
    /// turn in the background. Build with `--features webhooks`.
    Webhooks {
        /// Load config from sandboxes/<name>/config.toml instead of ~/.tengu/config.toml
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// Run skill evals against prompts.md/yaml and score pass/fail with an LLM judge.
    Eval {
        /// One or more skill names. Empty = discover all skills with evals.
        skills: Vec<String>,
        /// Override skill-local evals/config.toml with sandboxes/<name>/config.toml.
        #[arg(long)]
        sandbox: Option<String>,
        /// Judge model override. Default: anthropic/claude-opus-4-7.
        #[arg(long)]
        judge_model: Option<String>,
        /// Max rows run in parallel within a skill. Default: 1 (sequential).
        #[arg(long, default_value_t = 1)]
        concurrency: usize,
        /// Output format. Table prints a human summary; json prints the report JSON and suppresses the table.
        #[arg(long, default_value = "table")]
        format: String,
        /// Output directory for transcripts + report.json. Default: evals/runs/<ISO8601-ts>/.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Glob filter over row ids within a skill.
        #[arg(long)]
        filter: Option<String>,
        /// Keep per-row tmp workspaces after run (for debugging).
        #[arg(long)]
        keep_workspace: bool,
        /// Retain only the N most recent run directories under evals/runs/.
        /// Older ones are deleted at startup. Default 10. Ignored when
        /// --out is set. Pass a large number (e.g. 9999) to effectively disable.
        #[arg(long, default_value_t = 10)]
        keep_runs: usize,
        /// Skip writing per-row transcripts, report.json, metrics.json, and
        /// history.jsonl. The table / JSON summary still prints. For quick
        /// iteration without polluting the repo.
        #[arg(long)]
        no_persist: bool,
        /// Retain only the N most recent per-skill `metrics/runs/<ts>/`
        /// directories. `0` disables pruning. Default 10.
        #[arg(long, default_value_t = 10)]
        max_runs: u32,
    },
    /// Manage encrypted secrets vault in ~/.tengu/secrets.vault
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// Remove all cached/ephemeral state (conversations, memory, tasks, logs).
    Prune {
        /// Also prune workspace-local state for this sandbox
        #[arg(long)]
        sandbox: Option<String>,
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
        /// Hard reset: also remove each workspace's `.tengu/` dir (cache, daily
        /// logs, workspace skills), the top-level scaffold directories, and root
        /// runtime artifacts (TENGU_PLAN.md / TENGU_PLANNER_REGISTRY.md). The
        /// sandbox config is never touched. Requires --sandbox.
        #[arg(long)]
        hard: bool,
    },
    /// Run MCP bridge server (stdio). Used as a subprocess by Claude Code engine.
    McpBridge,
    /// Run a standalone MCP stdio server exposing the `agentic_memory` tool so
    /// external agents (ChatGPT / Codex / Claude) can share the Open Brain
    /// memory. Needs `TENGU_MEMORY_DATABASE_URL`; build with
    /// `--features postgres_memory`.
    AgenticMemoryServer,
    /// Skill lifecycle commands — evolve / metrics / list / install / etc.
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
    /// INTERNAL — subprocess mode invoked by SubprocessRunner. Not intended for
    /// direct user invocation. Refuses to run unless TENGU_AGENT_IPC=1 is set.
    /// Runs one plan step's LLM mini-loop (`run_agent_subprocess`) on the
    /// `[agents.<name>]` block of the parent's config.
    #[command(hide = true)]
    RunAgent,
}

#[derive(Subcommand)]
enum SecretAction {
    /// Create encrypted secrets vault with master password
    Init,
    /// Set a secret (KEY VALUE)
    Set { key: String, value: String },
    /// Remove a secret by key
    Remove { key: String },
    /// List secret key names (values hidden)
    List,
    /// Change the vault master password
    ChangePassword,
    /// Show the secrets file path
    Path,
}

#[derive(Subcommand)]
enum SkillAction {
    /// Bounded rewrite->rescore loop for a skill, with user approval gate.
    Evolve {
        skill: String,
        #[arg(long)]
        max_cycles: Option<u32>,
        #[arg(long)]
        target_metric: Option<String>,
        #[arg(long)]
        base_branch: Option<String>,
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// Inspect rolling metrics for a skill.
    Metrics {
        skill: String,
        #[arg(long, default_value_t = 10)]
        last: u32,
    },
    /// Apply a saved evolve proposal (reserved for future auto-trigger work).
    AcceptProposal { path: PathBuf },
    /// Remove a skill directory.
    Remove {
        name: String,
        /// Tier to remove from: project (default), workspace, or managed.
        #[arg(long, default_value = "project")]
        tier: String,
        #[arg(long)]
        yes: bool,
    },
    /// List skills across all three tiers.
    List {
        /// Filter to one tier; default is all three.
        #[arg(long)]
        tier: Option<String>,
    },
    /// Cross-check `[agents.*].skill_packages` refs vs filesystem; report orphans / phantoms.
    Doctor {
        /// Load config from sandboxes/<name>/config.toml instead of ~/.tengu/config.toml
        #[arg(long)]
        sandbox: Option<String>,
        /// Print findings only; do not exit non-zero on phantoms (CI use case).
        #[arg(long)]
        no_fail: bool,
    },
    /// Bundle a skill as a tar.gz artefact.
    Export {
        name: String,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Quarantine -> scan -> validate -> install a skill from a URL / git / local path.
    Install {
        source: String,
        #[arg(long, default_value = "managed")]
        tier: String,
        /// Refuse on caution/dangerous scan verdict.
        #[arg(long)]
        strict: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Teacher onboarding — drop a SKILL.md template and (optionally) copy
    /// a folder of teacher-provided materials into `skills/<name>/resources/`.
    /// When `resources_dir` is omitted, the resources/ folder is created
    /// empty with a short README; teachers populate it later via
    /// `adjust yourself` (the resource-finder agent fetches web sources)
    /// or by dropping files in directly.
    Seed {
        name: String,
        /// Optional directory of pre-staged materials. Each file becomes a
        /// resources/<filename>. Subdirs preserved. Omit to seed an empty
        /// resources/ folder.
        resources_dir: Option<PathBuf>,
        /// Tier to write to: project (default), workspace, or managed.
        #[arg(long, default_value = "project")]
        tier: String,
        /// Skill description for the SKILL.md frontmatter. Defaults to a
        /// helpful placeholder pointing at the resources/ dir.
        #[arg(long)]
        description: Option<String>,
        /// Mark the skill as `learner_facing: true` and `editable_by_learner: true`
        /// so the in-chat "adjust yourself" verb can mutate it.
        #[arg(long, default_value_t = true)]
        learner_facing: bool,
        #[arg(long)]
        yes: bool,
    },
}

pub(crate) async fn run() -> Result<()> {
    // Load .env first (highest priority after shell env), then parse CLI so we can
    // special-case subprocess modes that must keep stdout protocol-clean.
    dotenvy::dotenv().ok();
    let cli = Cli::parse();

    if matches!(cli.command, Some(Commands::McpBridge)) {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("tengu=info".parse().unwrap()),
            )
            .compact()
            .with_writer(std::io::stderr)
            .init();
        return crate::adapters::inbound::mcp_bridge::run_mcp_bridge().await;
    }

    #[cfg(feature = "postgres_memory")]
    if matches!(cli.command, Some(Commands::AgenticMemoryServer)) {
        // Stdout carries the JSON-RPC protocol — route tracing to stderr.
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("tengu=info".parse().unwrap()),
            )
            .compact()
            .with_writer(std::io::stderr)
            .init();
        return crate::adapters::inbound::mcp_bridge::run_agentic_memory_mcp_server().await;
    }

    if matches!(cli.command, Some(Commands::RunAgent)) {
        // Subprocess mode — stdout is JSON only, everything else goes to stderr.
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("tengu=info".parse().unwrap()),
            )
            .compact()
            .with_writer(std::io::stderr)
            .init();
        return run_agent_subprocess().await;
    }

    let tengu_home = resolve_tengu_home();
    let secrets_path = tengu_home.join("secrets.vault");
    let mut secret_registry = SecretRegistry::new();
    if secrets_path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&secrets_path) {
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    eprintln!(
                        "WARNING: {} has permissions {:o} — should be 600. \
                         Run: chmod 600 {}",
                        secrets_path.display(),
                        mode,
                        secrets_path.display()
                    );
                }
            }
        }
        match secrets::load_secrets_into_env(&secrets_path) {
            Ok(secret_values) => {
                for v in secret_values {
                    secret_registry.register(v);
                }
            }
            Err(e) => {
                eprintln!("WARNING: Failed to load secrets vault: {}", e);
            }
        }
    }
    // Also register the master password itself if set via env.
    if let Ok(pw) = std::env::var("TENGU_MASTER_PASSWORD") {
        if !pw.is_empty() {
            secret_registry.register(pw);
        }
    }
    let secret_registry = std::sync::Arc::new(secret_registry);

    // In TUI mode, persist logs to file only so interactive output stays clean.
    // In Telegram mode, log to both file and stderr so operators can monitor.
    let is_tui = matches!(cli.command, None | Some(Commands::Chat { .. }));
    let is_telegram = matches!(cli.command, Some(Commands::Telegram { .. }));
    // Webhook listener uses the same dual-output (file + stderr) pattern as
    // telegram so operators can `tail -f tengu.log` while also watching the
    // console for HMAC-fail / dispatch events.
    let is_webhooks = matches!(cli.command, Some(Commands::Webhooks { .. }));
    if is_tui {
        let log_dir = resolve_tengu_home().join("logs");
        std::fs::create_dir_all(&log_dir).ok();
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("tengu.log"))
            .expect("Failed to open log file");
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("tengu=info".parse().unwrap()),
            )
            .compact()
            .with_writer(std::sync::Mutex::new(log_file))
            .with_ansi(false)
            .init();
    } else if is_telegram || is_webhooks {
        use tracing_subscriber::layer::SubscriberExt;
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("tengu=info"));
        let log_dir = resolve_tengu_home().join("logs");
        std::fs::create_dir_all(&log_dir).ok();
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("tengu.log"))
            .expect("Failed to open log file");
        let file_layer = tracing_subscriber::fmt::layer()
            .compact()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(log_file));
        let stderr_layer = tracing_subscriber::fmt::layer()
            .compact()
            .with_writer(std::io::stderr);
        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .with(stderr_layer);
        tracing::subscriber::set_global_default(subscriber)
            .expect("Failed to set tracing subscriber");
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("tengu=info".parse().unwrap()),
            )
            .compact()
            .init();
    }

    let config_path = cli.config.unwrap_or_else(default_config_path);
    // `run-agent` children and the MCP bridge resolve the config through
    // `default_config_path` (`$TENGU_CONFIG` first) — pin it to the file this
    // process actually uses so `-c/--config` reaches them like `--sandbox`
    // does (via IPC).
    std::env::set_var("TENGU_CONFIG", &config_path);

    let config = if config_path.exists() {
        Config::load(&config_path)
            .with_context(|| format!("Failed to load config at {}", config_path.display()))?
    } else {
        info!(
            path = %config_path.display(),
            "Config file not found; using built-in defaults"
        );
        let config = Config::default();
        config.validate().with_context(|| {
            "Built-in default config failed validation; this is a runtime bug".to_string()
        })?;
        config
    };

    // Egress policy before anything builds an HTTP client or spawns a child;
    // `load_sandbox_or` re-installs from the sandbox config.
    crate::adapters::outbound::egress::install(&config.egress)?;

    let profile = RuntimeProfile::resolve(Some(&config.runtime_profile));

    match cli.command.unwrap_or(Commands::Chat { sandbox: None }) {
        Commands::Chat { sandbox } => {
            let config = load_sandbox_or(sandbox, config)?;
            tokio::task::block_in_place(|| {
                crate::adapters::inbound::tui::run_tui(config, profile, secret_registry)
            })
        }
        Commands::Status => {
            print_status(&config, profile);
            Ok(())
        }
        Commands::Doctor { sandbox, tor } => {
            let config = load_sandbox_or(sandbox, config)?;
            run_doctor(&config, tor).await
        }
        #[cfg(feature = "telegram")]
        Commands::Telegram { sandbox } => tokio::task::block_in_place(|| {
            let config = load_sandbox_or(sandbox, config)?;
            crate::adapters::inbound::telegram::run_telegram(config, secret_registry)
        }),
        #[cfg(not(feature = "telegram"))]
        Commands::Telegram { .. } => {
            anyhow::bail!("Telegram support requires: cargo build --features telegram")
        }
        #[cfg(feature = "webhooks")]
        Commands::Webhooks { sandbox } => {
            let config = load_sandbox_or(sandbox, config)?;
            crate::adapters::inbound::webhooks::run_webhooks(config, secret_registry).await
        }
        #[cfg(not(feature = "webhooks"))]
        Commands::Webhooks { .. } => {
            anyhow::bail!("webhook listener requires: cargo build --features webhooks")
        }
        Commands::Eval {
            skills,
            sandbox,
            judge_model,
            concurrency,
            format,
            out,
            filter,
            keep_workspace,
            keep_runs,
            no_persist,
            max_runs,
        } => {
            let format = match format.as_str() {
                "table" => crate::adapters::inbound::eval::OutputFormat::Table,
                "json" => crate::adapters::inbound::eval::OutputFormat::Json,
                other => anyhow::bail!("invalid --format: {} (expected 'table' or 'json')", other),
            };
            let args = crate::adapters::inbound::eval::EvalArgs {
                skills,
                sandbox,
                judge_model,
                concurrency,
                format,
                out_dir: out,
                filter,
                keep_workspace,
                keep_runs: Some(keep_runs),
                no_persist,
                max_per_run_reports: max_runs,
            };
            let exit_code = crate::adapters::inbound::eval::run(args).await?;
            std::process::exit(exit_code);
        }
        Commands::Prune { sandbox, yes, hard } => {
            let (workspaces, project_dirs): (Vec<PathBuf>, Vec<String>) =
                if let Some(ref name) = sandbox {
                    let config = load_sandbox_or(Some(name.clone()), config)?;
                    let ws = config
                        .agents
                        .values()
                        .filter_map(|a| {
                            a.workspace
                                .as_ref()
                                .map(|p| crate::config::paths::expand_tilde(p))
                        })
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect();
                    let project_dirs = config
                        .scaffold
                        .as_ref()
                        .and_then(|s| s.project.as_ref())
                        .map(|p| p.directories.clone())
                        .unwrap_or_default();
                    (ws, project_dirs)
                } else {
                    (Vec::new(), Vec::new())
                };
            if hard && sandbox.is_none() {
                eprintln!(
                    "--hard has no effect without --sandbox (no workspace to reset); \
                     pruning global state only."
                );
            }
            let targets = crate::adapters::outbound::prune::plan_prune(
                &crate::adapters::outbound::prune::PruneOptions {
                    tengu_home: &tengu_home,
                    workspaces: &workspaces,
                    project_dirs: &project_dirs,
                    hard,
                },
            );
            if targets.iter().all(|t| !t.exists) {
                println!("Nothing to prune.");
                return Ok(());
            }
            println!(
                "{}",
                crate::adapters::outbound::prune::format_prune_plan(&targets)
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
            let results = crate::adapters::outbound::prune::execute_prune(&targets);
            for (label, result) in &results {
                match result {
                    Ok(()) => println!("  ✓ {}", label),
                    Err(e) => println!("  ✗ {} — {}", label, e),
                }
            }
            Ok(())
        }
        Commands::McpBridge => crate::adapters::inbound::mcp_bridge::run_mcp_bridge().await,
        #[cfg(feature = "postgres_memory")]
        Commands::AgenticMemoryServer => {
            // Handled by the early-return in main() (stdout must stay
            // JSON-RPC-clean); this arm is for exhaustiveness only.
            unreachable!("Commands::AgenticMemoryServer is dispatched earlier in main()")
        }
        #[cfg(not(feature = "postgres_memory"))]
        Commands::AgenticMemoryServer => {
            anyhow::bail!(
                "agentic-memory MCP server requires: cargo build --features postgres_memory"
            )
        }
        Commands::Secret { action } => {
            let path = secrets::secrets_file_path(&resolve_tengu_home());
            match action {
                SecretAction::Init => secrets::init_secrets_file(&path)?,
                SecretAction::Set { key, value } => secrets::set_secret(&path, &key, &value)?,
                SecretAction::Remove { key } => secrets::remove_secret(&path, &key)?,
                SecretAction::List => {
                    let keys = secrets::list_secret_keys(&path)?;
                    if keys.is_empty() {
                        println!("  (no secrets)");
                    } else {
                        for k in &keys {
                            println!("  {}", k);
                        }
                    }
                }
                SecretAction::ChangePassword => secrets::change_password(&path)?,
                SecretAction::Path => println!("{}", path.display()),
            }
            Ok(())
        }
        Commands::Skill { action } => run_skill_command(config, action).await,
        Commands::RunAgent => {
            // Handled by the early-return in main(); this arm is for
            // exhaustiveness only.
            unreachable!("Commands::RunAgent is dispatched earlier in main()")
        }
    }
}
