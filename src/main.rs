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
    },
    /// Run MCP bridge server (stdio). Used as a subprocess by Claude Code engine.
    McpBridge,
    /// Bounded rewrite→rescore loop for a skill, with user approval gate.
    SkillEvolve {
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
    SkillMetrics {
        skill: String,
        #[arg(long, default_value_t = 10)]
        last: u32,
    },
    /// Apply a saved evolve proposal (reserved for future auto-trigger work).
    SkillAcceptProposal { path: PathBuf },
    /// Inspect and manage the RAG registry (tengu_registry + tengu_messages + tengu_outputs).
    /// Phase 1 of the redesign — see docs/IMPLEMENTATION_PLAN.md.
    Registry {
        #[command(subcommand)]
        action: RegistryAction,
    },
    /// INTERNAL — subprocess mode invoked by SubprocessRunner. Not intended for
    /// direct user invocation. Refuses to run unless TENGU_AGENT_IPC=1 is set.
    /// Phase 3 of the redesign ships a stub body; Phase 4 wires the real LLM
    /// mini-loop.
    #[command(hide = true)]
    RunAgent,
}

#[derive(Subcommand)]
enum RegistryAction {
    /// List entries in tengu_registry (type | name | score=N/A).
    List {
        /// Filter by entry type: "skill", "agent", or "tool".
        #[arg(long)]
        r#type: Option<String>,
        /// Max entries to list (by a broad search over the collection).
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Semantic search across tengu_registry. Prints top-K ranked hits.
    Search {
        /// Free-text query.
        query: String,
        /// How many hits to return.
        #[arg(long, default_value_t = 10)]
        top_k: usize,
    },
    /// Phase 1 bootstrap: re-index a small hardcoded set of placeholder tool
    /// descriptions so the registry has something to search. Phase 2 replaces
    /// this with real enumeration of compiled-in + MCP tool descriptions.
    ReindexTools,
    /// Phase 2: wipe tengu_registry, then re-index:
    ///   - `agents/*.toml`                    → kind = agent
    ///   - `skills/**/SKILL.md` (3-tier merge) → kind = skill
    ///   - Phase-1 placeholder tools           → kind = tool
    ReindexAll {
        /// Workspace root (defaults to cwd). agents/ and skills/ are looked
        /// up relative to this path.
        #[arg(long)]
        workspace: Option<PathBuf>,
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
            keep_runs,
            no_persist,
            max_runs,
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
                keep_runs: Some(keep_runs),
                no_persist,
                max_per_run_reports: max_runs,
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
        Commands::SkillEvolve {
            skill,
            max_cycles,
            target_metric,
            base_branch,
            sandbox,
        } => {
            let config = load_sandbox_or(sandbox, config)?;
            let workspace = std::env::current_dir()?;
            let chat_factory =
                adapters::channel_runtime::build_cli_chat_factory(&config, &workspace).await?;
            let args = adapters::skill_lifecycle::evolve::EvolveArgs {
                config: &config,
                workspace: &workspace,
                skill: &skill,
                max_cycles,
                target_metric,
                base_branch,
                chat_factory,
            };
            adapters::skill_lifecycle::evolve::run_evolve(args).await?;
            Ok(())
        }
        Commands::SkillMetrics { skill, last } => {
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
            let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&mj_path)?)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
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
        Commands::SkillAcceptProposal { path } => {
            eprintln!(
                "accept-proposal is a placeholder in v1. \
                 Proposals are applied inline during `tengu skill evolve`. \
                 Path ignored: {}",
                path.display()
            );
            Ok(())
        }
        Commands::Registry { action } => run_registry_command(&config, action).await,
        Commands::RunAgent => {
            // Handled by the early-return in main(); this arm is for
            // exhaustiveness only.
            unreachable!("Commands::RunAgent is dispatched earlier in main()")
        }
    }
}

/// `tengu run-agent` stub handler — Phase 3 scaffold.
///
/// **What this does today:**
/// 1. Verifies `TENGU_AGENT_IPC=1` is set (prevents accidental re-entry).
/// 2. Reads one JSON `AgentIpcInput` from stdin.
/// 3. Constructs a canned summary (no LLM).
/// 4. If the `qdrant` feature is on and `OPENROUTER_API_KEY` is set,
///    writes the summary to `tengu_outputs` via `compress_and_store`.
/// 5. Writes one `AgentIpcOutput` JSON line to stdout and exits 0.
///
/// Phase 4 replaces the canned step with a real LLM mini-loop.
async fn run_agent_subprocess() -> Result<()> {
    use tokio::io::AsyncReadExt;

    if std::env::var("TENGU_AGENT_IPC").ok().as_deref() != Some("1") {
        anyhow::bail!(
            "`tengu run-agent` is a subprocess mode not meant for direct invocation. \
             Set TENGU_AGENT_IPC=1 if you really want to run it (e.g. via scripts/test-runner.sh)."
        );
    }

    // Read stdin to EOF.
    let mut buf = Vec::new();
    tokio::io::stdin()
        .read_to_end(&mut buf)
        .await
        .context("read IPC input from stdin")?;
    let input: adapters::runner::AgentIpcInput = serde_json::from_slice(&buf)
        .context("parse IPC input JSON")?;

    tracing::info!(
        agent = %input.agent_name,
        session = %input.session_id,
        step = %input.step_id,
        "run-agent stub received"
    );

    // Phase 3 canned output: no LLM, no tool calls.
    let summary = format!(
        "[phase-3 stub] agent={} goal={} (LLM mini-loop lands in Phase 4)",
        input.agent_name, input.goal
    );
    let output_text = format!(
        "Phase 3 scaffold ran for agent `{}`. Goal received: {}\n\n\
         (No LLM was invoked. The subprocess IPC boundary is what is being tested.)",
        input.agent_name, input.goal
    );

    // Optional: write the summary to tengu_outputs if Qdrant is compiled in
    // and reachable. Failure is logged but does not break the IPC contract.
    #[cfg(feature = "qdrant")]
    {
        let config = load_config_or_default();
        match adapters::rag::RagStore::from_config(config.memory.clone()).await {
            Ok(rag) => {
                if let Err(e) = adapters::plugins::skill_lifecycle::compress_and_store::write_summary(
                    &rag,
                    &input.session_id,
                    &input.step_id,
                    &summary,
                )
                .await
                {
                    tracing::warn!(error = %e, "compress_and_store write failed (non-fatal for Phase 3 stub)");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "RagStore unavailable; skipping tengu_outputs write");
            }
        }
    }

    let out = adapters::runner::AgentIpcOutput::Ok {
        output: output_text,
        summary,
    };
    let json = serde_json::to_string(&out).context("serialise IPC output")?;
    println!("{}", json);
    Ok(())
}

/// Best-effort config load for the run-agent subprocess.
/// Falls back to defaults if the user's config.toml is absent or malformed.
#[cfg(feature = "qdrant")]
fn load_config_or_default() -> Config {
    let path = resolve_tengu_home().join("config.toml");
    if !path.is_file() {
        return Config::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).unwrap_or_else(|_| Config::default()),
        Err(_) => Config::default(),
    }
}

/// Dispatcher for `tengu registry ...` subcommands.
///
/// Phase 1 of the redesign: thin wrapper over `adapters::rag::RagStore`. Only
/// compiled when the `qdrant` feature is enabled — without it we bail with a
/// clear error so users know what's missing.
#[cfg(feature = "qdrant")]
async fn run_registry_command(config: &Config, action: RegistryAction) -> Result<()> {
    use crate::adapters::rag::{RagStore, REGISTRY_COLLECTION};

    let rag = RagStore::from_config(config.memory.clone())
        .await
        .context("failed to open RagStore — is Qdrant running and OPENROUTER_API_KEY set?")?;

    match action {
        RegistryAction::List { r#type, limit } => {
            // Phase 1 approximation: we don't have a native list-by-filter yet,
            // so we do a broad search with a neutral query. Phase 2 will add a
            // proper scroll. The type filter is applied client-side.
            let hits = rag.search_registry("list all", limit.max(1)).await?;
            let filtered: Vec<_> = hits
                .into_iter()
                .filter(|h| match &r#type {
                    Some(t) => h.kind.as_str() == t,
                    None => true,
                })
                .collect();
            println!("# tengu_registry ({} entries shown)", filtered.len());
            for h in filtered {
                println!(
                    "  [{:<5}] {:<32}  score={:.3}  {}",
                    h.kind.as_str(),
                    h.name,
                    h.score,
                    h.source_path.as_deref().unwrap_or("")
                );
            }
            Ok(())
        }
        RegistryAction::Search { query, top_k } => {
            let hits = rag.search_registry(&query, top_k).await?;
            println!(
                "# search '{}' in {} — top {}",
                query, REGISTRY_COLLECTION, top_k
            );
            for (i, h) in hits.iter().enumerate() {
                println!(
                    "{:>2}. [{:<5}] {:<32}  score={:.3}",
                    i + 1,
                    h.kind.as_str(),
                    h.name,
                    h.score
                );
                let snippet: String = h.description.chars().take(120).collect();
                println!("    {}", snippet);
            }
            Ok(())
        }
        RegistryAction::ReindexTools => {
            let n = rag.startup_index(placeholder_tools()).await?;
            println!("reindexed {} placeholder tool descriptions into tengu_registry", n);
            println!("(Phase 1 scaffold — Phase 2 also indexes agents/ and skills/ via `reindex-all`)");
            Ok(())
        }
        RegistryAction::ReindexAll { workspace } => {
            let root = workspace
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .context("could not resolve workspace root (pass --workspace)")?;
            let agents_dir = root.join("agents");
            let specs = crate::adapters::agents::load_agents_dir(&agents_dir)
                .with_context(|| format!("load agents from {}", agents_dir.display()))?;
            let skills = crate::adapters::rag::indexer::scan_skills(&root);

            rag.clear_registry().await?;
            let tool_count = rag.index_tools(placeholder_tools()).await?;
            let agent_count = rag.index_agents(specs.clone()).await?;
            let skill_count = rag.index_skills(skills.clone()).await?;

            println!("reindexed tengu_registry from {}", root.display());
            println!("  tools  : {} (placeholder set — Phase 3 will enumerate real ones)", tool_count);
            println!("  agents : {} (from {})", agent_count, agents_dir.display());
            for a in &specs {
                println!(
                    "           - {:<20} {}",
                    a.name,
                    a.source_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                );
            }
            println!("  skills : {} (3-tier scan)", skill_count);
            for s in &skills {
                println!("           - {:<20} {}", s.name, s.source_path.display());
            }
            Ok(())
        }
    }
}

/// Phase 2 placeholder tool descriptions. Phase 3 replaces this with a real
/// enumeration of compiled-in + MCP tools (the current tool registry is built
/// inside channel_runtime.rs — exposing it requires a small refactor that is
/// deliberately deferred).
#[cfg(feature = "qdrant")]
fn placeholder_tools() -> Vec<crate::adapters::types::ToolDef> {
    use crate::adapters::types::ToolDef;
    vec![
        ToolDef {
            name: "http_request".to_string(),
            description:
                "Perform an HTTP request (GET/POST/PUT/DELETE). Use for web scraping, \
                 REST API calls, fetching documents. Not for file I/O."
                    .to_string(),
            parameters: serde_json::json!({}),
        },
        ToolDef {
            name: "read_file".to_string(),
            description:
                "Read a file from the local workspace. Returns text content. \
                 Scoped to agent workspace by default."
                    .to_string(),
            parameters: serde_json::json!({}),
        },
        ToolDef {
            name: "list_directory".to_string(),
            description:
                "List the contents of a directory on the local workspace. Returns \
                 filenames and types. Scoped to agent workspace."
                    .to_string(),
            parameters: serde_json::json!({}),
        },
        ToolDef {
            name: "run_command".to_string(),
            description:
                "Run a shell command inside the agent workspace. For builds, tests, \
                 git operations, and other filesystem-local work."
                    .to_string(),
            parameters: serde_json::json!({}),
        },
        ToolDef {
            name: "remember".to_string(),
            description:
                "Store a short fact for cross-session recall via the memory provider. \
                 Use for user preferences and facts that should persist."
                    .to_string(),
            parameters: serde_json::json!({}),
        },
        ToolDef {
            name: "persistent_store".to_string(),
            description:
                "Store, search, list, or delete files against a semantic index. \
                 Use for longer-lived document storage that should be queryable by \
                 natural language."
                    .to_string(),
            parameters: serde_json::json!({}),
        },
    ]
}

/// Stub when the `qdrant` feature is off — `tengu registry` is a no-op and
/// tells the user how to rebuild with support.
#[cfg(not(feature = "qdrant"))]
async fn run_registry_command(_config: &Config, _action: RegistryAction) -> Result<()> {
    anyhow::bail!(
        "`tengu registry` requires the 'qdrant' cargo feature. \
         Rebuild with: cargo build --features qdrant"
    );
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
