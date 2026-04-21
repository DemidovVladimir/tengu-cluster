//! Tengu binary entry point and CLI chat runtime.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::info;

mod adapters;
use crate::adapters::config::{Config, RuntimeProfile};

use adapters::engine_builder::build_engine;
use adapters::secret_builder;
use adapters::secret_builder::SecretRegistry;

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
    Chat,
    /// Print static runtime status snapshot.
    Status,
    /// Run runtime/environment diagnostics.
    Doctor,
    /// Run Telegram bot adapter.
    Telegram {
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
    },
    /// Run MCP bridge server (stdio). Used as a subprocess by Claude Code engine.
    McpBridge,
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
        return adapters::mcp_bridge::run_mcp_bridge().await;
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
    let is_tui = matches!(cli.command, None | Some(Commands::Chat));
    let is_telegram = matches!(cli.command, Some(Commands::Telegram { .. }));
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
    } else if is_telegram {
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

    let config_path = cli
        .config
        .unwrap_or_else(|| resolve_tengu_home().join("config.toml"));

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

    let profile = RuntimeProfile::resolve(Some(&config.runtime_profile));

    match cli.command.unwrap_or(Commands::Chat) {
        Commands::Chat => {
            tokio::task::block_in_place(|| adapters::tui::run_tui(config, profile, secret_registry))
        }
        Commands::Status => {
            print_status(&config, profile);
            Ok(())
        }
        Commands::Doctor => {
            run_doctor(&config);
            Ok(())
        }
        #[cfg(feature = "telegram")]
        Commands::Telegram { sandbox } => tokio::task::block_in_place(|| {
            let config = load_sandbox_or(sandbox, config)?;
            adapters::telegram_builder::run_telegram(config, secret_registry)
        }),
        #[cfg(not(feature = "telegram"))]
        Commands::Telegram { .. } => {
            anyhow::bail!("Telegram support requires: cargo build --features telegram")
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
        } => {
            let format = match format.as_str() {
                "table" => adapters::eval_builder::OutputFormat::Table,
                "json" => adapters::eval_builder::OutputFormat::Json,
                other => anyhow::bail!("invalid --format: {} (expected 'table' or 'json')", other),
            };
            let args = adapters::eval_builder::EvalArgs {
                skills,
                sandbox,
                judge_model,
                concurrency,
                format,
                out_dir: out,
                filter,
                keep_workspace,
            };
            let exit_code = adapters::eval_builder::run(args).await?;
            std::process::exit(exit_code);
        }
        Commands::Prune { sandbox, yes } => {
            let (workspaces, scaffold_dirs): (Vec<PathBuf>, Vec<String>) =
                if let Some(ref name) = sandbox {
                    let config = load_sandbox_or(Some(name.clone()), config)?;
                    let ws = config
                        .agents
                        .values()
                        .filter_map(|a| {
                            a.workspace
                                .as_ref()
                                .map(|p| adapters::tool_builder::expand_tilde(p))
                        })
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect();
                    let dirs = config
                        .scaffold
                        .as_ref()
                        .and_then(|s| s.project.as_ref())
                        .map(|p| p.directories.clone())
                        .unwrap_or_default();
                    (ws, dirs)
                } else {
                    (Vec::new(), Vec::new())
                };
            let targets = adapters::prune::plan_prune(&tengu_home, &workspaces, &scaffold_dirs);
            if targets.iter().all(|t| !t.exists) {
                println!("Nothing to prune.");
                return Ok(());
            }
            println!("{}", adapters::prune::format_prune_plan(&targets));
            if !yes {
                eprint!("Proceed? [y/N] ");
                let mut buf = String::new();
                std::io::stdin().read_line(&mut buf)?;
                if !buf.trim().eq_ignore_ascii_case("y") {
                    println!("Aborted.");
                    return Ok(());
                }
            }
            let results = adapters::prune::execute_prune(&targets);
            for (label, result) in &results {
                match result {
                    Ok(()) => println!("  ✓ {}", label),
                    Err(e) => println!("  ✗ {} — {}", label, e),
                }
            }
            Ok(())
        }
        Commands::McpBridge => adapters::mcp_bridge::run_mcp_bridge().await,
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
    }
}

fn format_diagnostics_compact(d: &crate::adapters::EngineDiagnostics) -> String {
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

fn run_doctor(config: &Config) {
    println!();
    println!("  TENGU CLUSTER — Doctor");
    println!("  ─────────────────────────────────────");

    println!("  Backend diagnostics:");
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
            }
        }
    }

    println!("  ─────────────────────────────────────");
    println!();
}

/// Load a sandbox config if `--sandbox <name>` was given, otherwise use the default config.
///
/// Sandbox configs are loaded from `sandboxes/<name>/config.toml` relative to the
/// current working directory.
fn load_sandbox_or(sandbox: Option<String>, default: Config) -> Result<Config> {
    match sandbox {
        None => Ok(default),
        Some(name) => {
            let path = PathBuf::from("sandboxes").join(&name).join("config.toml");
            Config::load(&path).with_context(|| {
                format!("Failed to load sandbox '{}' from {}", name, path.display())
            })
        }
    }
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
