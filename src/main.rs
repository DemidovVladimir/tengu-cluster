//! Tengu binary entry point and CLI chat runtime.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::info;

mod adapters;
mod application;
mod domain;
#[cfg(test)]
mod main_tests;

use adapters::doctor_probe::{format_engine_diagnostics_compact, run_engine_probe};
use adapters::flow_store::FlowStore;
use tengu_core::config::{Config, RuntimeProfile};
#[cfg(test)]
use tengu_core::types::{Message, Role};

#[cfg(test)]
pub(crate) use adapters::doctor_probe::{first_output_line, resolve_models_probe_url};
#[cfg(test)]
pub(crate) use application::flow_compaction::compaction_split_index;
#[cfg(test)]
pub(crate) use application::flow_policy::{
    default_compaction_keep_turns_for_scope, default_compaction_threshold_ratio_for_scope,
    resolve_flow_compaction_policy,
};
#[cfg(test)]
pub(crate) use domain::chat::{
    default_history_turn_limit_for_scope, enforce_history_turn_limit, resolve_history_turn_limit,
};
#[cfg(test)]
pub(crate) use domain::usage::{absorb_turn_usage_snapshot, apply_turn_usage_to_session_totals};

use adapters::engine_factory::build_engine;
use adapters::secret_store;
use domain::secret_registry::SecretRegistry;

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
    /// Run multi-agent fleet orchestrator.
    Orchestrate {
        /// Load config from sandboxes/<name>/config.toml instead of ~/.tengu/config.toml
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// Run Telegram bot adapter.
    Telegram {
        /// Load config from sandboxes/<name>/config.toml instead of ~/.tengu/config.toml
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// Manage encrypted secrets vault in ~/.tengu/secrets.vault
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
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
    // Load encrypted secrets vault first (higher priority), then .env.
    // Shell env vars always win (load_secrets_into_env won't overwrite existing vars).
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
        match secret_store::load_secrets_into_env(&secrets_path) {
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
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    // In TUI mode, redirect logs to a file to avoid corrupting the alternate screen.
    // For other commands, log to stderr as usual.
    let is_tui = matches!(cli.command, None | Some(Commands::Chat));
    if is_tui {
        let log_dir = resolve_tengu_home().join("logs");
        std::fs::create_dir_all(&log_dir).ok();
        let log_file =
            std::fs::File::create(log_dir.join("tengu.log")).expect("Failed to create log file");
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("tengu=info".parse().unwrap()),
            )
            .compact()
            .with_writer(std::sync::Mutex::new(log_file))
            .with_ansi(false)
            .init();
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
        Commands::Chat => tokio::task::block_in_place(|| {
            adapters::tui::run_tui(config, profile, secret_registry)
        }),
        Commands::Status => {
            print_status(&config, profile);
            Ok(())
        }
        Commands::Doctor => {
            run_doctor(&config).await;
            Ok(())
        }
        Commands::Orchestrate { sandbox } => {
            let config = load_sandbox_or(sandbox, config)?;
            let event_bus = adapters::event_bus::InProcessEventBus::default();
            adapters::orchestrator::boot_orchestrator(&config, &event_bus, secret_registry).await
        }
        #[cfg(feature = "telegram")]
        Commands::Telegram { sandbox } => tokio::task::block_in_place(|| {
            let config = load_sandbox_or(sandbox, config)?;
            adapters::telegram_runtime::run_telegram(config, secret_registry)
        }),
        #[cfg(not(feature = "telegram"))]
        Commands::Telegram { .. } => {
            anyhow::bail!("Telegram support requires: cargo build --features telegram")
        }
        Commands::Secret { action } => {
            let path = secret_store::secrets_file_path(&resolve_tengu_home());
            match action {
                SecretAction::Init => secret_store::init_secrets_file(&path)?,
                SecretAction::Set { key, value } => secret_store::set_secret(&path, &key, &value)?,
                SecretAction::Remove { key } => secret_store::remove_secret(&path, &key)?,
                SecretAction::List => {
                    let keys = secret_store::list_secret_keys(&path)?;
                    if keys.is_empty() {
                        println!("  (no secrets)");
                    } else {
                        for k in &keys {
                            println!("  {}", k);
                        }
                    }
                }
                SecretAction::ChangePassword => secret_store::change_password(&path)?,
                SecretAction::Path => println!("{}", path.display()),
            }
            Ok(())
        }
    }
}

fn print_status(config: &Config, profile: RuntimeProfile) {
    println!();
    println!("  TENGU CLUSTER — Status");
    println!("  ─────────────────────────────────────");
    println!("  Profile:  {:?}", profile);
    println!("  Refiner:  {}", config.refiner.mode);
    println!("  Agents:   {}", config.agents.len());
    for (id, ac) in &config.agents {
        println!(
            "    - {} ({}/{}){}",
            id,
            ac.engine,
            ac.model,
            if ac.default { " [default]" } else { "" }
        );
        match build_engine(id, ac) {
            Ok(engine) => {
                let diagnostics = engine.diagnostics();
                println!(
                    "      diagnostics: {}",
                    format_engine_diagnostics_compact(&diagnostics)
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

async fn run_doctor(config: &Config) {
    println!();
    println!("  TENGU CLUSTER — Doctor");
    println!("  ─────────────────────────────────────");

    println!("  Backend diagnostics:");
    for (id, ac) in &config.agents {
        match build_engine(id, ac) {
            Ok(engine) => {
                let diagnostics = engine.diagnostics();
                println!(
                    "    {}: engine={} {}",
                    id,
                    diagnostics.engine_id,
                    format_engine_diagnostics_compact(&diagnostics)
                );

                let probe = run_engine_probe(&diagnostics)
                    .await
                    .unwrap_or_else(|| "probe: skipped (no provider probe configured)".to_string());
                println!("      {}", probe);
            }
            Err(err) => {
                println!("    {}: backend init error: {}", id, err);
            }
        }
    }

    let flow_store = FlowStore::new(&resolve_tengu_home());
    match flow_store {
        Ok(store) => {
            print!("  Flow store... ");
            match store.health_check() {
                Ok(_) => println!("OK"),
                Err(e) => {
                    println!("Error: {}", e);
                    println!("  ─────────────────────────────────────");
                    println!();
                    return;
                }
            }

            print!("  Flow integrity... ");
            match store.integrity_report() {
                Ok(report) if !report.has_issues() => {
                    println!("OK (checked {} flows)", report.checked_flows);
                }
                Ok(report) => {
                    println!("WARN");
                    print_flow_integrity_findings(&report);
                }
                Err(e) => println!("Error: {}", e),
            }
        }
        Err(e) => {
            print!("  Flow store... ");
            println!("Error: {}", e);
        }
    }

    println!("  ─────────────────────────────────────");
    println!();
}

fn print_flow_integrity_findings(report: &adapters::flow_store::FlowStoreIntegrityReport) {
    println!("    checked flows: {}", report.checked_flows);
    print_findings("missing transcripts", &report.missing_transcripts);
    print_findings("unsafe transcript paths", &report.unsafe_transcript_paths);
    print_findings("unreadable transcripts", &report.unreadable_transcripts);
    print_findings("invalid transcript lines", &report.invalid_transcript_lines);
    print_findings(
        "index/transcript metadata mismatches",
        &report.metadata_mismatches,
    );
    println!("    guidance:");
    println!("      1) Back up `~/.tengu/state/flows`.");
    println!("      2) Inspect listed flow entries and transcript files.");
    println!("      3) Repair or remove broken flow entries from `index.json` if needed.");
}

fn print_findings(label: &str, entries: &[String]) {
    if entries.is_empty() {
        return;
    }
    println!("    {}: {}", label, entries.len());
    for entry in entries.iter().take(3) {
        println!("      - {}", entry);
    }
    if entries.len() > 3 {
        println!("      - ... and {} more", entries.len() - 3);
    }
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
            Config::load(&path)
                .with_context(|| format!("Failed to load sandbox '{}' from {}", name, path.display()))
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
