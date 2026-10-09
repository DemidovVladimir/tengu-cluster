//! `tengu` CLI — clap definitions and command dispatch. `main.rs` only calls
//! [`run`]. Subcommands with real bodies live beside this file.

mod backtest;
mod decide;
mod doctor;
mod evidence;
mod history;
mod lineage;
mod risk;
mod run_agent;
mod skill;
mod studio;
mod tool;
mod trace;

use clap::{Parser, Subcommand};

use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::{info, warn};

use crate::adapters::outbound::secrets;
use crate::bootstrap::sandbox::load_sandbox_or;
use crate::config::paths::{default_config_path, resolve_tengu_home};
use crate::config::{Config, RuntimeProfile};
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
        /// Also check the sandbox's running `tengu run`: fail when its
        /// heartbeat is missing or older than `[runtime]
        /// heartbeat_stale_secs`, or a required feed is down or stale
        /// (Docker healthcheck).
        #[arg(long)]
        live: bool,
        /// Also run a tool-using smoke turn (list_directory + read_file in
        /// a temp workspace) on every agent's own engine + model and print
        /// agent | engine | model | ok | tools called | secs; fail on any
        /// failed turn. Calls the models (costs tokens). On macOS a `local`
        /// agent with a loopback base_url is skipped, never contacted.
        #[arg(long)]
        engines: bool,
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
    /// turn in the background. Takes the leases `tengu run` takes (never
    /// beside it); SIGINT / SIGTERM drain it. Build with `--features webhooks`.
    Webhooks {
        /// Load config from sandboxes/<name>/config.toml instead of ~/.tengu/config.toml
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// Run the sandbox's long-running process: every `[decision_loops.*]`
    /// built once, the webhook routes (with `--features webhooks` and
    /// `[webhooks] enabled`), one runner per sandbox (lease), graceful
    /// shutdown on SIGINT / SIGTERM. See docs/runtime-2026-09-30.md.
    Run {
        /// Load config from sandboxes/<name>/config.toml instead of ~/.tengu/config.toml
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// Run one event through a `[decision_loops.<name>]` loop (Jev picks,
    /// tools execute) and print the step outcomes. No escalation — use
    /// `tengu webhooks` with an endpoint `loop = "<name>"` for that.
    Decide {
        /// Load config from sandboxes/<name>/config.toml instead of ~/.tengu/config.toml
        #[arg(long)]
        sandbox: Option<String>,
        /// Loop name (`[decision_loops.<name>]`); optional with `--map` (the
        /// map names its loop).
        #[arg(long = "loop", required_unless_present = "map")]
        loop_name: Option<String>,
        /// Event JSON file (`-` = stdin). Omitted = `{}`.
        #[arg(long, conflicts_with = "map")]
        event: Option<PathBuf>,
        /// Execution map JSON file (`-` = stdin): a higher-order agent's run
        /// of one loop — order, event, tighter caps — that can only narrow
        /// the sandbox TOML (`config/execution_map.rs`). Carries its event.
        #[arg(long)]
        map: Option<PathBuf>,
    },
    /// Tengu Studio (TENGU_STUDIO_PLAN.md): with no subcommand, the local
    /// browser UI (`--features studio`): loopback only, prints its URL (with
    /// a per-process token) on stdout; Play / Stop / send-event only with
    /// `[studio] control = true` or `--allow-control`. `graph` prints the
    /// sandbox's workflow graph — validated config + catalog tools as
    /// nodes and edges, optionally narrowed by an execution map — as JSON.
    Studio {
        /// Load config from sandboxes/<name>/config.toml instead of ~/.tengu/config.toml
        #[arg(long, global = true)]
        sandbox: Option<String>,
        #[command(flatten)]
        serve: studio::ServeArgs,
        #[command(subcommand)]
        action: Option<studio::StudioAction>,
    },
    /// Read the execution trace `tengu run` / `tengu decide` recorded under
    /// <TENGU_HOME>/logs/trace/<sandbox>/: `runs`, `show --run <id>
    /// [--after <seq>] [--follow]`. JSON lines; no config, read-only.
    Trace {
        /// Sandbox name (the trace directory; `default` for a -c config).
        #[arg(long, global = true)]
        sandbox: Option<String>,
        #[command(subcommand)]
        action: trace::TraceAction,
    },
    /// Read recorded observation history (`[recorder]`): `range` / `asof`,
    /// JSON lines with full keys. Fill and inspect the market-data
    /// warehouse `<state dir>/market.db` (xlab): `backfill` (Hyperliquid,
    /// GeckoTerminal), `events` (SEC EDGAR filings), `import-hl-archive`,
    /// `import-json`, `coverage`.
    History {
        /// Load config from sandboxes/<name>/config.toml instead of ~/.tengu/config.toml
        #[arg(long, global = true)]
        sandbox: Option<String>,
        #[command(subcommand)]
        action: history::HistoryAction,
    },
    /// Backtest a strategy spec on the market-data warehouse
    /// <state dir>/market.db (xlab, no LLM): a [backtest.strategies] name
    /// or a JSON spec file; the research arm (+ the [risk]-capped arm), an
    /// optional in-sample / holdout split. Prints the summary and writes the
    /// run dir <state dir>/backtests/<run id>/. See docs/xlab-2026-10-01.md
    /// § 6, § 10.
    Backtest {
        /// Load config from sandboxes/<name>/config.toml instead of ~/.tengu/config.toml
        #[arg(long)]
        sandbox: Option<String>,
        #[command(flatten)]
        args: backtest::BacktestArgs,
    },
    /// Preserve and grade forward evidence (docs/lineage-2026-10-06.md § 3):
    /// `snapshot` a record into a read-only vault, `verify` it, recorder
    /// `coverage`, `grade` a paper ledger, `regrade` rule W from recorded
    /// books. No config, no network; every reader is read-only.
    Evidence {
        #[command(subcommand)]
        action: evidence::EvidenceAction,
    },
    /// The lineage registry (`lineage/`): verify it, trace a record, a
    /// family's search accounting, the Rule-W acceptance report,
    /// capabilities, generations, seal a preregistration. No config needed.
    /// See docs/lineage-2026-10-06.md § 5.
    Lineage(lineage::LineageArgs),
    /// Paper-ledger risk state of a `[risk]` sandbox: `status` (read-only),
    /// `halt` / `resume` (operator at a terminal only; resume asks for the
    /// account name, and for the content of `TENGU_RISK_RESUME_SECRET_FILE`
    /// when that names a 0600 file). See docs/xmarket-risk-paper-2026-09-30.md.
    Risk {
        /// Load config from sandboxes/<name>/config.toml instead of ~/.tengu/config.toml
        #[arg(long, global = true)]
        sandbox: Option<String>,
        #[command(subcommand)]
        action: risk::RiskAction,
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
        /// Hard reset: empty each workspace root — every file and directory in
        /// it (its `.tengu/` cache and workspace skills, agent output, anything
        /// else) — plus the top-level scaffold directories and the root
        /// TENGU_PLAN.md / TENGU_PLANNER_REGISTRY.md. The workspace roots, the
        /// sandbox config and `<TENGU_HOME>/state` stay. Requires --sandbox.
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
    /// INTERNAL — run one catalog tool in-process as `[agents.<name>]` and
    /// print `{text, observation, is_error}` (`tool call`), or list every
    /// catalog tool (`tool list`). The in-process half of
    /// `tests/bridge_conformance.rs`; see `cli/tool.rs`.
    #[command(hide = true)]
    Tool {
        #[command(subcommand)]
        action: tool::ToolAction,
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

    if let Some(Commands::Tool { action }) = cli.command {
        // stdout is one JSON value; logs go to stderr.
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("tengu=info".parse().unwrap()),
            )
            .compact()
            .with_writer(std::io::stderr)
            .init();
        return tool::run_tool_command(cli.config, action).await;
    }

    if let Some(Commands::Evidence { action }) = cli.command {
        // No config, secrets or egress: read-only files + the new vault.
        // stdout carries the report; logs go to stderr.
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("tengu=info".parse().unwrap()),
            )
            .compact()
            .with_writer(std::io::stderr)
            .init();
        return tokio::task::block_in_place(|| evidence::run_evidence(action));
    }

    if let Some(Commands::Lineage(args)) = cli.command {
        // Registry only — no config, no secrets; stdout carries the view.
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("tengu=info".parse().unwrap()),
            )
            .compact()
            .with_writer(std::io::stderr)
            .init();
        return lineage::run_lineage(args);
    }

    if let Some(Commands::Trace { sandbox, action }) = cli.command {
        // Trace files only — no config, no secrets; stdout carries JSON lines.
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("tengu=info".parse().unwrap()),
            )
            .compact()
            .with_writer(std::io::stderr)
            .init();
        return trace::run_trace(sandbox, action).await;
    }

    let tengu_home = resolve_tengu_home();
    // Inherited vault names (`TENGU_SECRETS_LOADED`), else the vault itself,
    // plus the master password — the same registry `tengu mcp-bridge` and
    // `run-agent` build (without the vault prompt). `tengu secret` opens the
    // vault itself — unlocking it here too asked for the password twice.
    let vault =
        loads_vault_at_startup(&cli.command).then(|| secrets::secrets_file_path(&tengu_home));
    let secret_registry = std::sync::Arc::new(secrets::process_secret_registry(vault.as_deref()));

    // In TUI mode, persist logs to file only so interactive output stays clean.
    // In Telegram mode, log to both file and stderr so operators can monitor.
    let is_tui = matches!(cli.command, None | Some(Commands::Chat { .. }));
    let is_telegram = matches!(cli.command, Some(Commands::Telegram { .. }));
    // Webhook listener, `tengu run` and the Studio server (its Play runs the
    // `tengu run` runtime in-process) use the same dual-output (file +
    // stderr) pattern as telegram so operators can `tail -f tengu.log` while
    // also watching the console for HMAC-fail / dispatch events. Never
    // stdout: the Studio server prints its URL there alone.
    let is_webhooks = matches!(
        cli.command,
        Some(
            Commands::Webhooks { .. }
                | Commands::Run { .. }
                | Commands::Studio { action: None, .. }
        )
    );
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
    } else if stdout_is_data(&cli.command) {
        // stdout carries JSON (lines) / the operator's report; logs go to
        // stderr, so `tengu decide … | jq` reads the JSON alone.
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("tengu=info".parse().unwrap()),
            )
            .compact()
            .with_writer(std::io::stderr)
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

    let config_path = cli.config.unwrap_or_else(default_config_path);
    // `run-agent` children and the MCP bridge resolve the config through
    // `default_config_path` (`$TENGU_CONFIG` first) — pin it to the file this
    // process actually uses so `-c/--config` reaches them like `--sandbox`
    // does (via IPC).
    std::env::set_var("TENGU_CONFIG", &config_path);

    let config = if config_path.exists() {
        match (Config::load(&config_path), replacing_sandbox(&cli.command)) {
            (Ok(config), _) => config,
            // `--sandbox` replaces the base config wholesale: a broken base
            // file must not stop a sandbox command.
            (Err(e), Some(sandbox)) => {
                warn!(
                    path = %config_path.display(),
                    sandbox,
                    error = %format!("{e:#}"),
                    "base config does not load; ignored — --sandbox replaces it"
                );
                Config::default()
            }
            (Err(e), None) => {
                return Err(e)
                    .with_context(|| format!("Failed to load config at {}", config_path.display()))
            }
        }
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
        Commands::Doctor {
            sandbox,
            tor,
            live,
            engines,
        } => {
            let config = load_sandbox_or(sandbox, config)?;
            run_doctor(&config, tor, live, engines).await
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
        Commands::Run { sandbox } => {
            let config = load_sandbox_or(sandbox, config)?;
            crate::adapters::inbound::run::run_runtime(config, secret_registry).await
        }
        Commands::Decide {
            sandbox,
            loop_name,
            event,
            map,
        } => {
            let config = load_sandbox_or(sandbox, config)?;
            decide::run_decide(
                &config,
                loop_name.as_deref(),
                event.as_deref(),
                map.as_deref(),
                secret_registry,
            )
            .await
        }
        Commands::Studio {
            sandbox,
            serve,
            action,
        } => {
            let config = load_sandbox_or(sandbox, config)?;
            studio::run_studio(config, serve, action, secret_registry).await
        }
        Commands::History { sandbox, action } => {
            let config = load_sandbox_or(sandbox, config)?;
            history::run_history(&config, action).await
        }
        Commands::Risk { sandbox, action } => {
            let config = load_sandbox_or(sandbox, config)?;
            risk::run_risk(&config, action).await
        }
        Commands::Backtest { sandbox, args } => {
            let config = load_sandbox_or(sandbox, config)?;
            backtest::run_backtest(&config, args).await
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
        Commands::Lineage(_) => {
            unreachable!("Commands::Lineage is dispatched earlier in run()")
        }
        Commands::Tool { .. } => {
            unreachable!("Commands::Tool is dispatched earlier in main()")
        }
        Commands::Evidence { .. } => {
            unreachable!("Commands::Evidence is dispatched earlier in main()")
        }
        Commands::Trace { .. } => {
            unreachable!("Commands::Trace is dispatched earlier in run()")
        }
    }
}

/// The `--sandbox` of a command whose config is that sandbox file alone
/// (`load_sandbox_or` replaces the base config wholesale), so the base config
/// is not needed to run it.
fn replacing_sandbox(command: &Option<Commands>) -> Option<&str> {
    match command.as_ref()? {
        Commands::Chat { sandbox }
        | Commands::Telegram { sandbox }
        | Commands::Webhooks { sandbox }
        | Commands::Run { sandbox }
        | Commands::Doctor { sandbox, .. }
        | Commands::Decide { sandbox, .. }
        | Commands::Studio { sandbox, .. }
        | Commands::History { sandbox, .. }
        | Commands::Backtest { sandbox, .. }
        | Commands::Risk { sandbox, .. } => sandbox.as_deref(),
        _ => None,
    }
}

/// Commands whose stdout is data — JSON (lines) or the doctor's report —
/// so their log lines go to stderr. (`tool`, `evidence`, `lineage`,
/// `trace`, `run-agent` and the MCP servers set stderr up earlier.)
fn stdout_is_data(command: &Option<Commands>) -> bool {
    matches!(
        command,
        Some(
            Commands::History { .. }
                | Commands::Risk { .. }
                | Commands::Backtest { .. }
                | Commands::Decide { .. }
                | Commands::Doctor { .. }
                | Commands::Studio { .. }
        )
    )
}

/// Whether startup unlocks the secrets vault: every command but `tengu
/// secret`, which opens it itself (one password prompt, not two).
fn loads_vault_at_startup(command: &Option<Commands>) -> bool {
    !matches!(command, Some(Commands::Secret { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(args: &[&str]) -> Option<Commands> {
        Cli::try_parse_from(args).expect("parse").command
    }

    /// A broken base config is ignored only when `--sandbox` replaces it.
    #[test]
    fn only_sandbox_commands_skip_the_base_config() {
        assert_eq!(
            replacing_sandbox(&command(&["tengu", "chat", "--sandbox", "lping"])),
            Some("lping")
        );
        assert_eq!(
            replacing_sandbox(&command(&["tengu", "run", "--sandbox", "xmarket"])),
            Some("xmarket")
        );
        assert_eq!(replacing_sandbox(&command(&["tengu", "chat"])), None);
        assert_eq!(replacing_sandbox(&command(&["tengu", "status"])), None);
        assert_eq!(replacing_sandbox(&command(&["tengu"])), None);
    }

    /// ST-03 finding: `decide` / `doctor` printed log lines on stdout before
    /// their JSON / report. Every data command now logs to stderr.
    #[test]
    fn data_commands_log_to_stderr() {
        for args in [
            &[
                "tengu",
                "decide",
                "--sandbox",
                "control-loop-lab",
                "--loop",
                "demo",
            ][..],
            &["tengu", "doctor", "--sandbox", "control-loop-lab", "--live"],
            &["tengu", "studio", "graph", "--sandbox", "control-loop-lab"],
            // The server: its URL alone on stdout.
            &[
                "tengu",
                "studio",
                "--sandbox",
                "control-loop-lab",
                "--port",
                "0",
            ],
            &["tengu", "history", "range", "k", "--from", "0", "--to", "1"],
        ] {
            assert!(stdout_is_data(&command(args)), "{args:?}");
        }
        assert!(!stdout_is_data(&command(&[
            "tengu",
            "run",
            "--sandbox",
            "x"
        ])));
        assert!(!stdout_is_data(&command(&["tengu", "chat"])));
        // `trace` sets up stderr logging itself, before any config.
        assert!(matches!(
            command(&[
                "tengu",
                "trace",
                "show",
                "--sandbox",
                "s",
                "--run",
                "r",
                "--after",
                "3"
            ]),
            Some(Commands::Trace { .. })
        ));
    }

    #[test]
    fn tengu_secret_opens_the_vault_itself() {
        assert!(!loads_vault_at_startup(&command(&[
            "tengu", "secret", "list"
        ])));
        assert!(!loads_vault_at_startup(&command(&[
            "tengu", "secret", "set", "K", "V"
        ])));
        assert!(loads_vault_at_startup(&command(&["tengu", "chat"])));
        assert!(loads_vault_at_startup(&command(&["tengu"])));
    }
}
