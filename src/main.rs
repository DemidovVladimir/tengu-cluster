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
    Orchestrate,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present (non-fatal if missing).
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
        Commands::Chat => tokio::task::block_in_place(|| adapters::tui::run_tui(config, profile)),
        Commands::Status => {
            print_status(&config, profile);
            Ok(())
        }
        Commands::Doctor => {
            run_doctor(&config).await;
            Ok(())
        }
        Commands::Orchestrate => {
            let event_bus = tengu_core::events::InProcessEventBus::default();
            adapters::orchestrator::boot_orchestrator(&config, &event_bus).await
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
