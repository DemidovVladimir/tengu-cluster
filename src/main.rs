//! Tengu binary entry point and CLI chat runtime.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use tracing::info;

mod adapters;
mod application;
mod domain;
mod ports;
use crate::adapters::config::{Config, RuntimeProfile};

use crate::adapters::engine_builder::build_engine;
use crate::adapters::secret_builder;
use crate::adapters::secret_builder::SecretRegistry;

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

#[tokio::main]
async fn main() -> Result<()> {
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
        return crate::adapters::mcp_bridge::run_mcp_bridge().await;
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
        return crate::adapters::mcp_bridge::run_agentic_memory_mcp_server().await;
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
        match secret_builder::load_secrets_into_env(&secrets_path) {
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
    crate::adapters::egress::install(&config.egress)?;

    let profile = RuntimeProfile::resolve(Some(&config.runtime_profile));

    match cli.command.unwrap_or(Commands::Chat { sandbox: None }) {
        Commands::Chat { sandbox } => {
            let config = load_sandbox_or(sandbox, config)?;
            tokio::task::block_in_place(|| {
                crate::adapters::tui::run_tui(config, profile, secret_registry)
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
            crate::adapters::telegram_builder::run_telegram(config, secret_registry)
        }),
        #[cfg(not(feature = "telegram"))]
        Commands::Telegram { .. } => {
            anyhow::bail!("Telegram support requires: cargo build --features telegram")
        }
        #[cfg(feature = "webhooks")]
        Commands::Webhooks { sandbox } => {
            let config = load_sandbox_or(sandbox, config)?;
            crate::adapters::webhook_builder::run_webhooks(config, secret_registry).await
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
                "table" => crate::adapters::eval_builder::OutputFormat::Table,
                "json" => crate::adapters::eval_builder::OutputFormat::Json,
                other => anyhow::bail!("invalid --format: {} (expected 'table' or 'json')", other),
            };
            let args = crate::adapters::eval_builder::EvalArgs {
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
            let exit_code = crate::adapters::eval_builder::run(args).await?;
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
                                .map(|p| crate::adapters::tool_builder::expand_tilde(p))
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
            let targets =
                crate::adapters::prune::plan_prune(&crate::adapters::prune::PruneOptions {
                    tengu_home: &tengu_home,
                    workspaces: &workspaces,
                    project_dirs: &project_dirs,
                    hard,
                });
            if targets.iter().all(|t| !t.exists) {
                println!("Nothing to prune.");
                return Ok(());
            }
            println!("{}", crate::adapters::prune::format_prune_plan(&targets));
            if !yes {
                eprint!("Proceed? [y/N] ");
                let mut buf = String::new();
                std::io::stdin().read_line(&mut buf)?;
                if !buf.trim().eq_ignore_ascii_case("y") {
                    println!("Aborted.");
                    return Ok(());
                }
            }
            let results = crate::adapters::prune::execute_prune(&targets);
            for (label, result) in &results {
                match result {
                    Ok(()) => println!("  ✓ {}", label),
                    Err(e) => println!("  ✗ {} — {}", label, e),
                }
            }
            Ok(())
        }
        Commands::McpBridge => crate::adapters::mcp_bridge::run_mcp_bridge().await,
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
            let path = secret_builder::secrets_file_path(&resolve_tengu_home());
            match action {
                SecretAction::Init => secret_builder::init_secrets_file(&path)?,
                SecretAction::Set { key, value } => {
                    secret_builder::set_secret(&path, &key, &value)?
                }
                SecretAction::Remove { key } => secret_builder::remove_secret(&path, &key)?,
                SecretAction::List => {
                    let keys = secret_builder::list_secret_keys(&path)?;
                    if keys.is_empty() {
                        println!("  (no secrets)");
                    } else {
                        for k in &keys {
                            println!("  {}", k);
                        }
                    }
                }
                SecretAction::ChangePassword => secret_builder::change_password(&path)?,
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

#[cfg(feature = "postgres_memory")]
async fn try_persist_agentic_step_summary(
    parent_config: &crate::adapters::config::Config,
    session_id: &str,
    step_id: &str,
    summary: &str,
) -> anyhow::Result<String> {
    let embedding = match std::env::var("OPENROUTER_API_KEY") {
        Ok(api_key) => {
            let embedder = crate::adapters::memory::vector::Embedder::new(
                api_key,
                parent_config.memory.embedding_model.clone(),
            );
            match embedder.embed(summary).await {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "agentic_memory: step summary embedding failed; writing text-only memory"
                    );
                    None
                }
            }
        }
        Err(_) => None,
    };
    crate::adapters::plugins::agentic_memory::write_step_summary_with_embedding(
        session_id,
        step_id,
        summary,
        embedding.as_deref(),
    )
    .await
}

/// `tengu run-agent` handler — Phase 5b (real LLM + multi-turn tool loop).
///
/// 1. Verifies `TENGU_AGENT_IPC=1` is set (prevents accidental re-entry).
/// 2. Reads one JSON `AgentIpcInput` from stdin.
/// 3. Loads the parent's config (sandbox via IPC, else the default config) and
///    takes `[agents.<name>]` from it — model, engine, tools, skills, limits.
/// 4. Composes the system prompt: base template + skill bodies (three-tier
///    loader) + mandatory `compress_and_store` suffix.
/// 5. Builds the engine (`engine` = `openrouter` | `claude_code`) for the agent's model.
/// 6. Builds the tool stack: `effective_tools = (base ∩ agent.tools) ∪ {compress_and_store}`
///    plus a `PluginToolExecutor` over those tools.
/// 7. Drives a multi-turn loop: per turn, drain stream → if tool_calls,
///    dispatch each → append assistant + tool messages → repeat. Stop on:
///    - empty tool_calls (model done)
///    - `compress_and_store` invoked (capture summary, exit clean)
///    - the agent's `limits.max_tool_rounds` exceeded (return Failed status)
/// 8. Emit one `AgentIpcOutput` JSON line on stdout and exit.
async fn run_agent_subprocess() -> Result<()> {
    use crate::domain::message::{Message, Role};
    use crate::ports::engine::EngineContext;
    use tokio::io::AsyncReadExt;

    if std::env::var("TENGU_AGENT_IPC").ok().as_deref() != Some("1") {
        anyhow::bail!(
            "`tengu run-agent` is a subprocess mode not meant for direct invocation. \
             Set TENGU_AGENT_IPC=1 if you really want to run it (e.g. via tests/run_agent_ipc.rs)."
        );
    }

    // Read stdin to EOF.
    let mut buf = Vec::new();
    tokio::io::stdin()
        .read_to_end(&mut buf)
        .await
        .context("read IPC input from stdin")?;
    let input: crate::adapters::runner::AgentIpcInput =
        serde_json::from_slice(&buf).context("parse IPC input JSON")?;

    tracing::info!(
        agent = %input.agent_name,
        session = %input.session_id,
        step = %input.step_id,
        "run-agent received"
    );

    // Phase 7.6 Bug B — expose session_id via env so plugins (notably
    // `agentic_memory` `capture`) can stamp it on their writes without needing it
    // threaded through ToolCtx. Set BEFORE building the tool executor so any
    // plugin construction that reads it sees the right value.
    std::env::set_var("TENGU_SESSION_ID", &input.session_id);

    // ----- Resolve parent config (Phase 7.2 sandbox inheritance) -----
    //
    // The child runs on the SAME config as the parent: `sandbox_config`
    // names the sandbox (`sandboxes/<name>/config.toml`), otherwise the
    // default config chain applies. `load_sandbox_or` returns `Err` only
    // when the file exists but fails to parse; we fall back to the default
    // config in that case (warn-and-continue).
    let parent_config = match load_sandbox_or(
        input.sandbox_config.clone(),
        load_config_or_default_unconditional(),
    ) {
        Ok(cfg) => {
            if let Some(ref name) = input.sandbox_config {
                tracing::info!(
                    sandbox = %name,
                    "subprocess loaded sandbox config (Phase 7.2)"
                );
            }
            cfg
        }
        Err(e) => {
            tracing::warn!(
                sandbox = ?input.sandbox_config,
                error = %e,
                "subprocess failed to load sandbox config; falling back to default"
            );
            load_config_or_default_unconditional()
        }
    };

    // Parent's `[egress]` (TENGU_EGRESS) wins; an invalid policy aborts the
    // child rather than running tools unproxied.
    crate::adapters::egress::install(&parent_config.egress)
        .context("run-agent: install egress policy")?;

    // ----- Resolve the agent: `[agents.<name>]` of that config -----
    //
    // Phase 6.7 (C→B B-half): when `input.compose` is set, the parent has
    // composed a transient agent. Take the BASE block
    // `[agents.<compose.base_agent>]` (not `[agents.<input.agent_name>]`,
    // which may be a synthetic label for events/logs), then override the
    // base's `skills` and `tools` with the values the planner picked from
    // the planner registry roster. The override is in-memory only — the config
    // on disk is unchanged.
    let (spec_load_name, compose_override) = match &input.compose {
        Some(c) => {
            tracing::info!(
                base = %c.base_agent,
                label = %input.agent_name,
                skill_override_count = c.skills.len(),
                tool_override_count = c.tools.len(),
                "run-agent: composed agent (C→B B-half)"
            );
            (c.base_agent.clone(), Some(c.clone()))
        }
        None => (input.agent_name.clone(), None),
    };
    // Only routable blocks (with a `description`) may run as a plan step —
    // the same rule `SubprocessRunner::run_step` and the registry apply.
    let mut spec = parent_config
        .agents
        .get(&spec_load_name)
        .filter(|a| a.description.is_some())
        .cloned()
        .with_context(|| {
            let mut known: Vec<&str> = parent_config
                .agents
                .iter()
                .filter(|(_, a)| a.description.is_some())
                .map(|(n, _)| n.as_str())
                .collect();
            known.sort();
            format!(
                "no agent `{}` in the active config (sandbox: {}); routable agents (with a `description`): {:?}",
                spec_load_name,
                input.sandbox_config.as_deref().unwrap_or("default config"),
                known
            )
        })?;
    // `~` in `workspace` is expanded by every in-process consumer; do the
    // same here so scopes / memory / the engine agree on one absolute path.
    spec.workspace = spec
        .workspace
        .as_ref()
        .map(|p| crate::adapters::tool_builder::expand_tilde(p));
    if let Some(c) = compose_override {
        spec.skill_packages = c.skills;
        spec.tools = c.tools;
    }
    // Resolved agent name (the base for composed agents) — exposed like
    // TENGU_SESSION_ID so plugins can attribute writes without ToolCtx plumbing.
    std::env::set_var("TENGU_AGENT_NAME", &spec_load_name);

    // IPC `model` overrides the agent block's `model` when non-empty (the
    // orchestrator can swap models per-step in the future).
    let model = if input.model.is_empty() {
        spec.model.clone()
    } else {
        input.model.clone()
    };

    // ----- Compose system prompt: base + skill bodies + suffix -----
    let mut system_prompt = String::from(BASE_AGENT_TEMPLATE);
    for skill_name in &spec.skill_packages {
        match load_skill_body_three_tier(skill_name) {
            Some(body) => {
                system_prompt.push_str("\n\n---\n\n");
                system_prompt.push_str(&body);
            }
            None => {
                tracing::warn!(skill = %skill_name, "skill body not found in any tier");
            }
        }
    }
    // Plan block: the parent's per-session `plan_state` IPC field is the
    // source of truth; the global `TENGU_PLAN.md` is only a fallback for old
    // parents that don't send it (it is overwritten by every session).
    let plan_state = match input.plan_state.as_deref() {
        Some(rendered) => crate::adapters::orchestrator::shared_files::plan_state_block(
            rendered,
            "IPC `plan_state`",
        ),
        None => crate::adapters::orchestrator::shared_files::read_plan_state_block(
            &std::env::current_dir()?,
        ),
    };
    if !plan_state.is_empty() {
        system_prompt.push_str("\n\n---\n\n");
        system_prompt.push_str(&plan_state);
    }
    system_prompt.push_str(MANDATORY_SUFFIX);

    // ----- Build engine (Phase 7.3 — honour the agent's engine) -----
    let workspace = spec
        .workspace
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut agent_cfg_for_engine = crate::adapters::channel_runtime::subagent_config(&spec);
    // The Claude Code engine ships these scopes to the MCP bridge; the child
    // workspace must be an allowed fs root there too.
    crate::adapters::channel_runtime::grant_workspace_root(
        &mut agent_cfg_for_engine.scopes,
        &workspace,
    );
    let engine = crate::adapters::engine_builder::build_engine(
        &input.agent_name,
        &agent_cfg_for_engine,
        parent_config.claude_code.as_ref(),
    )
    .with_context(|| {
        format!(
            "build engine for agent {} (engine={}, model={})",
            input.agent_name, spec.engine, model
        )
    })?;
    tracing::info!(
        agent = %input.agent_name,
        engine = %spec.engine,
        model = %model,
        "subprocess engine built"
    );
    let stream_event_timeout_secs = spec.limits.stream_event_timeout_secs;

    // ----- Build tool stack (Phase 5b) -----
    let secret_registry =
        std::sync::Arc::new(crate::adapters::secret_builder::SecretRegistry::new());
    let activity: std::sync::Arc<dyn crate::ports::tool_activity::ToolActivityPort> =
        std::sync::Arc::new(SubprocessActivity);
    // Phase 7.6 Bug A — build a real MemoryManager from the parent config so
    // the MemoryPlugin can register persistent_store / memory_ingest as
    // callable handlers (not just advertised tool defs). Without this,
    // MCP-routed Claude Code calls to those tools fail with
    // "Tool 'X' is not available to this agent" even though the tool def is
    // in the advertised list.
    let memory_manager = if parent_config.memory.enabled {
        Some(
            crate::adapters::channel_runtime::build_memory_manager_async(
                &parent_config.memory,
                Some(&workspace),
            )
            .await,
        )
    } else {
        None
    };

    let (tools, executor) = crate::adapters::channel_runtime::build_subprocess_tool_executor(
        &spec,
        &parent_config,
        &workspace,
        &secret_registry,
        activity,
        memory_manager.clone(),
    );
    // Diagnostic: log the actual tool NAMES the subprocess can call, so we
    // can verify (in the parent log) whether expected tools like
    // `persistent_store` made it through the `[agents.<name>].tools` allow-list +
    // workspace_tools opt-in machinery. Critical for debugging "agent says
    // tool unavailable" symptoms — without this we have no visibility into
    // the subprocess's tool world.
    let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    tracing::info!(
        agent = %input.agent_name,
        engine = %spec.engine,
        tool_count = tools.len(),
        tools = ?tool_names,
        "subprocess tool stack built"
    );

    // ----- Build messages + context -----
    let mut messages = vec![
        Message {
            role: Role::System,
            content: system_prompt.clone(),
            tool_call_id: None,
            tool_calls: None,
        },
        Message {
            role: Role::User,
            content: input.goal.clone(),
            tool_call_id: None,
            tool_calls: None,
        },
    ];
    // Phase 7.4 — when running on Claude Code engine, expose tengu's plugin
    // tools to the CLI via its MCP bridge. Without this the Claude CLI only
    // has its built-in tools (Read/Write/Edit/Bash) and treats tengu tools
    // (http_request, compress_and_store, persistent_store, etc.) as unknown
    // — agents end up calling them as bash commands and failing.
    //
    // OpenRouter path leaves bridge_tools = None (the ToolDef list is
    // registered through the OpenAI-compatible function-calling API
    // instead, handled by `tools` passed to run_single_engine_turn).
    let bridge_tools_for_ctx: Option<Vec<crate::domain::message::ToolDef>> =
        if spec.engine == "claude_code" {
            Some(tools.clone())
        } else {
            None
        };
    let context = EngineContext {
        workspace: spec.workspace.clone(),
        system_prompt: Some(system_prompt),
        bridge_tools: bridge_tools_for_ctx,
        max_tool_rounds: Some(input.max_turns),
        max_mcp_result_chars: Some(spec.limits.max_mcp_result_chars),
    };

    // ----- Multi-turn loop (Phase 5b) -----
    let mut final_text = String::new();
    let mut summary: Option<String> = None;
    let mut compress_called = false;
    // Per-turn metrics records — shipped back to the parent in the IPC
    // output so they can be re-emitted on the parent's metrics bus.
    let mut subagent_metrics: Vec<crate::adapters::metrics::MetricsRecord> = Vec::new();

    for turn in 0..input.max_turns {
        // Compute the prompt size BEFORE the engine call so the metric
        // record reflects what we sent. UTF-8-aware char count + byte len.
        let prompt_chars: u32 = messages
            .iter()
            .map(|m| m.content.chars().count() as u32)
            .sum();
        let prompt_bytes: u32 = messages.iter().map(|m| m.content.len() as u32).sum();
        let turn_started = std::time::Instant::now();

        let (text, tool_calls, input_delta, output_delta) =
            crate::adapters::engine_builder::run_single_engine_turn(
                engine.as_ref(),
                &messages,
                &tools,
                &context,
                None,
                stream_event_timeout_secs,
            )
            .await
            .context("engine turn failed")?;

        // Record one metric per engine turn. `input_delta`/`output_delta`
        // come from `StreamEvent::Usage` frames (OpenRouter + Claude Code
        // both supply them); they're 0 when the engine doesn't return usage.
        let rec = crate::adapters::metrics::MetricsRecord {
            ts_unix: crate::adapters::metrics::now_unix(),
            session_id: input.session_id.clone(),
            kind: crate::adapters::metrics::MetricsKind::Subagent,
            agent: input.agent_name.clone(),
            model: model.clone(),
            prompt_tokens: input_delta,
            completion_tokens: output_delta,
            total_tokens: input_delta.saturating_add(output_delta),
            prompt_chars,
            prompt_bytes,
            response_chars: text.chars().count() as u32,
            latency_ms: turn_started.elapsed().as_millis() as u64,
            layers: Vec::new(),
            step_id: Some(input.step_id.clone()),
        };
        // Emit into the subprocess's own tracing log too (the parent forwards
        // stderr — Phase 7.5) so the user sees the same line whether they
        // grep the parent log or a future subprocess log file.
        crate::adapters::metrics::record(rec.clone());
        subagent_metrics.push(rec);

        // If no tool calls, the model produced its final answer. Save the
        // text and exit the loop.
        if tool_calls.is_empty() {
            final_text = text;
            break;
        }

        // Append the assistant message carrying the tool_calls so the next
        // engine turn sees the full call/result history.
        messages.push(Message {
            role: Role::Assistant,
            content: text.clone(),
            tool_call_id: None,
            tool_calls: Some(tool_calls.clone()),
        });
        if !text.is_empty() {
            final_text = text;
        }

        // Dispatch each tool call.
        for call in &tool_calls {
            let result = if call.name == "compress_and_store" {
                // Out-of-band handling: capture the summary here; with
                // `postgres_memory` it is persisted to Postgres `agentic_memory`.
                let extracted_summary = call
                    .arguments
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                summary = Some(extracted_summary.clone());
                compress_called = true;
                #[cfg(feature = "postgres_memory")]
                {
                    let _ = try_persist_agentic_step_summary(
                        &parent_config,
                        &input.session_id,
                        &input.step_id,
                        &extracted_summary,
                    )
                    .await;
                }
                "stored".to_string()
            } else if let Some(ref exec) = executor {
                use crate::ports::engine::ToolExecutor;
                match exec.execute(call, &messages).await {
                    Ok(s) => s,
                    Err(e) => format!("tool error: {}", e),
                }
            } else {
                format!("tool '{}' is not available in this subprocess", call.name)
            };

            messages.push(Message {
                role: Role::Tool,
                content: result,
                tool_call_id: Some(call.id.clone()),
                tool_calls: None,
            });
        }

        if compress_called {
            tracing::info!(
                turn,
                "compress_and_store invoked; exiting subagent loop cleanly"
            );
            break;
        }

        if turn + 1 >= input.max_turns {
            tracing::warn!(
                turn,
                max_turns = input.max_turns,
                "subagent loop hit max_turns without compress_and_store"
            );
        }
    }

    // If the model never called compress_and_store, treat the final
    // assistant text as the summary (graceful degradation, same as Phase 5a).
    let summary = summary.unwrap_or_else(|| final_text.clone());

    // Backstop the durable write when the subagent finished WITHOUT calling
    // compress_and_store — the path Claude Code subagents most often take
    // (they "just stop" after their last tool call). Fail-soft: errors are
    // logged and swallowed; the IPC payload still goes back to the parent
    // unchanged — recall is best-effort, not a barrier to step completion.
    #[cfg(feature = "postgres_memory")]
    if !compress_called && !summary.trim().is_empty() {
        match try_persist_agentic_step_summary(
            &parent_config,
            &input.session_id,
            &input.step_id,
            &summary,
        )
        .await
        {
            Ok(id) => tracing::info!(
                entry_id = %id,
                session_id = %input.session_id,
                step_id = %input.step_id,
                summary_chars = summary.chars().count(),
                "agentic_memory: backstop wrote final_text summary to Postgres"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                session_id = %input.session_id,
                step_id = %input.step_id,
                "agentic_memory: backstop write FAILED"
            ),
        }
    }

    // Choose the user-visible `output` text. Models that only emit tool
    // calls (no inline assistant text) leave `final_text` empty; in that
    // case the summary the model produced via compress_and_store is the
    // most useful thing to show.
    let output = if !final_text.is_empty() {
        final_text.clone()
    } else if !summary.is_empty() {
        summary.clone()
    } else {
        String::new()
    };

    // Phase 5c — middle-ground protocol enforcement.
    // Pass:    compress_and_store called              → Ok (the canonical good path)
    // Pass:    no compress_and_store but text produced → Ok (model answered usefully)
    // Fail:    no compress_and_store AND no text       → Failed (DagExecutor retries)
    //
    // The strict-doctrine version of REDESIGN §7 would flip the second row
    // to Failed too; we deliberately stay pragmatic — many models produce
    // good answers in pure-text turns without calling the protocol tool.
    if !compress_called {
        tracing::warn!(
            agent = %input.agent_name,
            "model finished without calling compress_and_store"
        );
    }
    let out = if compress_called || !final_text.is_empty() {
        crate::adapters::runner::AgentIpcOutput::Ok {
            output,
            summary,
            metrics: subagent_metrics,
        }
    } else {
        // Genuinely empty run — no text, no protocol call, no useful output.
        // Surface as Failed so the orchestrator can retry / replan.
        crate::adapters::runner::AgentIpcOutput::Failed {
            error: format!(
                "subagent '{}' produced no output and did not call compress_and_store",
                input.agent_name
            ),
            output,
            metrics: subagent_metrics,
        }
    };
    let json = serde_json::to_string(&out).context("serialise IPC output")?;
    println!("{}", json);
    Ok(())
}

/// Subprocess `ToolActivityPort` impl — silent. The parent runner sees
/// progress via the engine's StreamEvent::TextDelta path, not via this hook.
struct SubprocessActivity;
impl crate::ports::tool_activity::ToolActivityPort for SubprocessActivity {
    fn publish_tool_activity(&self, _call: &crate::domain::message::ToolCall) {}
}

/// Config loader used by the `run-agent` subprocess path — needs
/// `parent_config.default_scopes` regardless of which memory features are
/// compiled in.
/// The `run-agent` child's base config: the same file the parent resolved
/// (`$TENGU_CONFIG`, pinned by `main`), loaded with env substitution and
/// validation. Built-in defaults only when there is no file; a file that
/// fails to load is an error worth seeing, not a silent `Config::default()`.
fn load_config_or_default_unconditional() -> Config {
    let path = default_config_path();
    if !path.is_file() {
        return Config::default();
    }
    match Config::load(&path) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!(
                path = %path.display(),
                error = %format!("{e:#}"),
                "run-agent: config failed to load; falling back to built-in defaults (no agents from it)"
            );
            Config::default()
        }
    }
}

/// Hardcoded base prompt that applies to every subagent. Kept short — the
/// real per-agent character comes from the loaded skill bodies.
const BASE_AGENT_TEMPLATE: &str = "You are a focused subagent run as a single-shot process. \
Read the user's goal, do exactly what is asked, and respond concisely with the result. \
Do not ask follow-up questions — make reasonable assumptions and answer the user directly.";

/// Mandatory suffix appended to every subagent system prompt — Phase 5b
/// version. Tells the model to call `compress_and_store(summary)` as its
/// final action; the harness captures the summary (persisted to Postgres
/// `agentic_memory` with `postgres_memory`) and exits the loop on that call.
const MANDATORY_SUFFIX: &str = "\n\n---\n\n\
When you have completed your task, your FINAL action MUST be to call the \
`compress_and_store` tool with a concise `summary` of what you accomplished. \
Failure to call it will be treated as task failure.";

/// Three-tier skill loader: workspace root → workspace dotdir → managed
/// (~/.tengu/skills). Returns the SKILL.md body with frontmatter stripped,
/// from the FIRST tier that has the file (highest precedence wins).
fn load_skill_body_three_tier(name: &str) -> Option<String> {
    let mut candidates: Vec<std::path::PathBuf> = vec![
        std::path::PathBuf::from("skills")
            .join(name)
            .join("SKILL.md"),
        std::path::PathBuf::from(".tengu")
            .join("skills")
            .join(name)
            .join("SKILL.md"),
    ];
    if let Some(home) = dirs_next::home_dir() {
        candidates.push(
            home.join(".tengu")
                .join("skills")
                .join(name)
                .join("SKILL.md"),
        );
    }

    for path in &candidates {
        if !path.is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "read SKILL.md failed");
                continue;
            }
        };
        // Strip frontmatter `---<yaml>---` if present.
        if content.starts_with("---") {
            let after_first = &content[3..];
            if let Some(end) = after_first.find("\n---") {
                let mut body_start = end + 4;
                let bytes = after_first.as_bytes();
                if body_start < bytes.len() && bytes[body_start] == b'\n' {
                    body_start += 1;
                }
                return Some(after_first[body_start..].trim_start().to_string());
            }
        }
        return Some(content);
    }
    None
}

// Memory inspection: Open Brain (Postgres `agentic_memory`) is the only durable
// memory backend. Inspect it with SQL against `TENGU_MEMORY_DATABASE_URL` or
// the ignored `postgres_*_smoke` tests. A Postgres-native inspect CLI is a
// tracked follow-up in docs/SESSION_HANDOFF.md.

// ---------------------------------------------------------------------------
// `tengu skill ...` dispatcher and handlers (Batch 2 of skill-research-2026-04-28)
// ---------------------------------------------------------------------------

use crate::adapters::skill_lifecycle::{audit, scanner};

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

async fn run_skill_command(config: Config, action: SkillAction) -> Result<()> {
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
                crate::adapters::channel_runtime::build_cli_chat_factory(&config, &workspace)
                    .await?;
            let args = crate::adapters::skill_lifecycle::evolve::EvolveArgs {
                config: &config,
                workspace: &workspace,
                skill: &skill,
                max_cycles,
                target_metric,
                base_branch,
                chat_factory,
            };
            crate::adapters::skill_lifecycle::evolve::run_evolve(args).await?;
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
                crate::adapters::skill_lifecycle::storage::MetricsJson,
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
    let shell = crate::adapters::shell_executor::LocalShellExecutor::new();
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

    let shell = crate::adapters::shell_executor::LocalShellExecutor::new();
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
/// `skill_distill` (`src/adapters/plugins/skill_lifecycle/distill.rs:147–204`).
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
    // `src/adapters/plugins/skill_lifecycle/distill.rs:148`.
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
    let stub = crate::adapters::skill_lifecycle::fixtures::FixturesFile {
        schema_version: 1,
        fixtures: vec![crate::adapters::skill_lifecycle::fixtures::Fixture {
            id: "f1".to_string(),
            prompt: "<TODO: a typical question a learner would ask>".to_string(),
            expected_tool_calls: Vec::new(),
            expected_outcome: Some(String::new()),
            metrics: Vec::new(),
        }],
    };
    crate::adapters::skill_lifecycle::fixtures::write_fixtures(
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

fn format_diagnostics_compact(d: &crate::ports::engine::EngineDiagnostics) -> String {
    let caps = &d.capabilities;
    format!(
        "model={} endpoint={} transport={} context={} output_cap={} streaming={}",
        d.configured_model.as_deref().unwrap_or("n/a"),
        d.endpoint.as_deref().unwrap_or("n/a"),
        d.transport.as_deref().unwrap_or("n/a"),
        caps.context_window,
        caps.max_output_tokens_per_turn,
        caps.supports_streaming,
    )
}

fn print_status(config: &Config, profile: RuntimeProfile) {
    println!();
    println!("  TENGU CLUSTER — Status");
    println!("  ─────────────────────────────────────");
    println!("  Profile:  {:?}", profile);
    println!("  Agents:   {}", config.agents.len());
    for (id, ac) in &config.agents {
        println!(
            "    - {} ({}/{}){}",
            id,
            ac.engine,
            ac.model,
            if ac.default { " [default]" } else { "" }
        );
        match build_engine(id, ac, config.claude_code.as_ref()) {
            Ok(engine) => {
                let diagnostics = engine.diagnostics();
                println!(
                    "      diagnostics: {}",
                    format_diagnostics_compact(&diagnostics)
                );
            }
            Err(err) => {
                println!("      diagnostics: unavailable ({})", err);
            }
        }
    }
    println!("  Hub:      {}:{}", config.hub.bind, config.hub.port);
    println!("  ─────────────────────────────────────");
    println!();
}

/// `tengu doctor` — build every configured agent's engine and print its
/// diagnostics. Returns `Err` (→ non-zero exit) when any engine fails to
/// build; the Docker `HEALTHCHECK` relies on that exit code.
async fn run_doctor(config: &Config, tor_check: bool) -> Result<()> {
    println!();
    println!("  TENGU CLUSTER — Doctor");
    println!("  ─────────────────────────────────────");

    println!("  Backend diagnostics:");
    let mut failures: Vec<String> = Vec::new();
    for (id, ac) in &config.agents {
        match build_engine(id, ac, config.claude_code.as_ref()) {
            Ok(engine) => {
                let diagnostics = engine.diagnostics();
                println!(
                    "    {}: engine={} {}",
                    id,
                    diagnostics.engine_id,
                    format_diagnostics_compact(&diagnostics)
                );
            }
            Err(err) => {
                println!("    {}: backend init error: {}", id, err);
                failures.push(format!("{id}: {err}"));
            }
        }
    }

    doctor_egress(tor_check, &mut failures).await;

    println!("  ─────────────────────────────────────");
    println!();

    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "doctor: {} check(s) failed:\n{}",
            failures.len(),
            failures
                .iter()
                .map(|f| format!("  - {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

/// `[egress]` block of `tengu doctor`: prints the installed policy, checks
/// the proxy port accepts TCP, and with `--tor` asks check.torproject.org
/// (through the tool client) whether traffic exits via Tor.
async fn doctor_egress(tor_check: bool, failures: &mut Vec<String>) {
    let policy = crate::adapters::egress::policy();
    let cfg = policy.config();
    println!("  Egress:");
    println!("    network: {}", policy.network());
    println!(
        "    proxy: {}",
        cfg.proxy.as_deref().unwrap_or("none (direct)")
    );
    println!(
        "    llm api: {}",
        if policy.route_llm_api() {
            "via proxy"
        } else {
            "direct"
        }
    );
    println!(
        "    hosts: allow={:?} deny={:?} https_only={}",
        cfg.allow_hosts, cfg.deny_hosts, cfg.https_only
    );
    let shell = match (policy.is_isolated_shell(), cfg.proxy.is_some()) {
        (true, _) => "isolated (sandbox-exec: only the proxy port)",
        (false, true) => "proxy_env (advisory — programs may ignore it)",
        (false, false) => "direct",
    };
    println!("    shell: {shell}");
    println!(
        "    audit: {}",
        policy
            .audit_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "off".to_string())
    );

    if let Some(url) = cfg
        .proxy
        .as_deref()
        .and_then(|p| reqwest::Url::parse(p).ok())
    {
        let host = url
            .host_str()
            .unwrap_or("")
            .trim_matches(|c| c == '[' || c == ']');
        let addr = format!("{}:{}", host, url.port().unwrap_or(0));
        let connect = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::net::TcpStream::connect(addr.as_str()),
        )
        .await;
        match connect {
            Ok(Ok(_)) => println!("    proxy port: {addr} reachable"),
            Ok(Err(e)) => {
                println!("    proxy port: {addr} UNREACHABLE ({e})");
                failures.push(format!(
                    "egress: proxy {addr} unreachable ({e}) — is tor running?"
                ));
            }
            Err(_) => {
                println!("    proxy port: {addr} UNREACHABLE (timeout)");
                failures.push(format!("egress: proxy {addr} connect timed out"));
            }
        }
    }

    if tor_check {
        match tor_exit_check(&policy).await {
            Ok((true, ip)) => println!("    tor: IsTor=true exit={ip}"),
            Ok((false, ip)) => {
                println!("    tor: IsTor=false exit={ip}");
                failures.push(format!("egress: traffic exits at {ip}, NOT via Tor"));
            }
            Err(e) => {
                println!("    tor: check failed ({e:#})");
                failures.push(format!("egress: tor check failed: {e:#}"));
            }
        }
    }
}

async fn tor_exit_check(policy: &crate::adapters::egress::EgressPolicy) -> Result<(bool, String)> {
    let body: serde_json::Value = policy
        .tool_client(std::time::Duration::from_secs(60))?
        .get("https://check.torproject.org/api/ip")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok((
        body["IsTor"].as_bool().unwrap_or(false),
        body["IP"].as_str().unwrap_or("unknown").to_string(),
    ))
}

/// Load a sandbox config if `--sandbox <name>` was given, otherwise use the default config.
///
/// Sandbox configs are loaded from `sandboxes/<name>/config.toml` relative to the
/// current working directory. Phase 7.2 — when a sandbox config is loaded, the
/// `Config.sandbox_name` runtime-only field is set to the resolved name so
/// downstream code (notably `SubprocessRunner` → `tengu run-agent` IPC) can
/// re-resolve the same config in the child process. Without this, child
/// subagents fall through to the default user config and lose sandbox-specific
/// scopes/secrets/MCP servers — concretely, http_request scope-denies in the
/// child even when the parent's sandbox allows it.
fn load_sandbox_or(sandbox: Option<String>, default: Config) -> Result<Config> {
    let cfg = match sandbox {
        None => default,
        Some(name) => {
            let path = PathBuf::from("sandboxes").join(&name).join("config.toml");
            let mut cfg = Config::load(&path).with_context(|| {
                format!("Failed to load sandbox '{}' from {}", name, path.display())
            })?;
            cfg.sandbox_name = Some(name);
            crate::adapters::egress::install(&cfg.egress)?;
            cfg
        }
    };
    // The effective policy is known only now (sandbox wins over the base
    // config). Children inherit it via `TENGU_EGRESS` and stay quiet — the
    // parent already printed the warning.
    if std::env::var_os("TENGU_AGENT_IPC").is_none() {
        crate::adapters::egress::policy().warn_if_proxy_unreachable();
    }
    Ok(cfg)
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
